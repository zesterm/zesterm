//! The chrome layout pass: model in, rectangles + text runs + hit map out.
//!
//! Pure on purpose. A `GlyphInstance` needs atlas coordinates that only exist
//! GPU-side, so text leaves here as [`TextRun`] specs that the redraw resolves
//! through `zest_render_wgpu::emit_ui_run`; rectangles are finished
//! [`RectInstance`]s. The hit map is built from the *same* rectangles in the
//! same pass — the property the tests pin is that clicking the middle of a
//! drawn tab returns that tab, always.
//!
//! Text width is injected (`measure`), because shaping needs the font stack:
//! the app passes `measure_ui_run`, the tests pass arithmetic.

use zest_config::settings::TabsPosition;
use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::{ChromeHitMap, HitRegion};
use super::model::{ChromeMetrics, ChromeModel, TabModel, TabOrigin, TabPresence};
use super::theme::ChromeColors;

/// One run of UI text, to be shaped and emitted at redraw.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub text: String,
    /// Baseline-left origin, physical pixels.
    pub pos: [f32; 2],
    /// Truncate with an ellipsis past this.
    pub max_width: f32,
    pub color: LinearRgba,
    pub clip: [f32; 4],
}

/// The finished chrome for one frame.
#[derive(Debug, Default)]
pub struct ChromeLayout {
    pub rects: Vec<RectInstance>,
    pub texts: Vec<TextRun>,
    pub hit: ChromeHitMap,
    /// `model.strip_scroll`, clamped into the range the content allows. The
    /// app stores this back so the scroll cannot wander off the end of the
    /// strip while tabs close.
    pub strip_scroll: f32,
    /// The picker's scroll, clamped likewise.
    pub picker_scroll: f32,
    /// The shortcuts sheet's scroll, clamped likewise.
    pub shortcuts_scroll: f32,
    /// The settings overlay's scroll, clamped — and possibly *adjusted*, when
    /// the model asked for the selection to be brought into view.
    pub settings_scroll: f32,
}

// Logical-pixel constants, scaled at use. Named because the tests reason
// about them; not settings, because nobody should have to care.
const TAB_MIN: f32 = 120.0;
const TAB_MAX: f32 = 220.0;
const TAB_VPAD: f32 = 4.0;
const TAB_HPAD: f32 = 2.0;
const TEXT_PAD: f32 = 8.0;
const RADIUS: f32 = 6.0;
const CLOSE: f32 = 16.0;
const NEW_TAB: f32 = 24.0;
const PILL_HPAD: f32 = 5.0;
const HAIRLINE: f32 = 1.0;
const EDGE_PAD: f32 = 8.0;
const ROW_H: f32 = 44.0;
const ROW_HPAD: f32 = 6.0;
const HEADER_MIN: f32 = 28.0;
const LINE_GAP: f32 = 2.0;

pub fn layout(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
) -> ChromeLayout {
    let mut out = match model.position {
        TabsPosition::Top => horizontal(model, colors, m, measure),
        TabsPosition::Left => vertical(model, colors, m, measure),
    };
    if let Some(picker) = &model.picker {
        // Appended last on purpose: last drawn is topmost, and last pushed
        // wins the hit lookup — the same fact, stated once.
        picker_overlay(picker, colors, m, measure, &mut out);
    }
    if let Some(shortcuts) = &model.shortcuts {
        shortcuts_overlay(shortcuts, colors, m, measure, &mut out);
    }
    if let Some(settings) = &model.settings {
        settings_overlay(settings, colors, m, measure, &mut out);
    }
    out
}

// Picker geometry, logical px.
const PICKER_W: f32 = 560.0;
const PICKER_H: f32 = 420.0;
const PICKER_MARGIN: f32 = 40.0;
const PICKER_PAD: f32 = 12.0;
const PICKER_ROW_H: f32 = 30.0;
const PICKER_RADIUS: f32 = 10.0;
const PICKER_INDENT: f32 = 18.0;

fn picker_overlay(
    picker: &super::model::PickerModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
    out: &mut ChromeLayout,
) {
    use super::model::PickerRow;

    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    // The scrim swallows every click that is not a row: the grid must not
    // hear a stray press while a modal list is up.
    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::PickerScrim);

    let w = (PICKER_W * s).min(m.width - PICKER_MARGIN * s);
    let h = (PICKER_H * s).min(m.height - PICKER_MARGIN * s);
    let panel = [(m.width - w) / 2.0, (m.height - h) / 2.5, w, h];
    let mut panel_rect = RectInstance::rounded(panel, PICKER_RADIUS * s, colors.panel_bg, no_clip);
    panel_rect.shadow_blur = 24.0 * s;
    panel_rect.shadow_alpha = colors.shadow_alpha;
    out.rects.push(panel_rect);

    // The filter line. An empty filter shows a hint rather than nothing —
    // an unlabeled empty box reads as broken.
    let filter_h = m.line_height + 2.0 * PICKER_PAD * s;
    let (filter_text, filter_color) = if picker.filter.is_empty() {
        ("attach to a session, or start one".to_string(), colors.text_faint)
    } else {
        (picker.filter.clone(), colors.text_active)
    };
    out.texts.push(TextRun {
        text: filter_text,
        pos: [panel[0] + PICKER_PAD * s, text_baseline(m, panel[1], filter_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color: filter_color,
        clip: panel,
    });
    out.rects.push(RectInstance::filled(
        [panel[0], panel[1] + filter_h, w, HAIRLINE * s],
        colors.line,
        no_clip,
    ));

    // Rows, scrolled and clipped inside the panel below the filter line.
    let rows_clip = [
        panel[0],
        panel[1] + filter_h + HAIRLINE * s,
        w,
        h - filter_h - HAIRLINE * s,
    ];
    let row_h = PICKER_ROW_H * s;
    let content_h = picker.rows.len() as f32 * row_h;
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let scroll = picker.scroll.clamp(0.0, max_scroll);
    out.picker_scroll = scroll;

    for (i, row) in picker.rows.iter().enumerate() {
        let y = rows_clip[1] + i as f32 * row_h - scroll;
        let rect = [panel[0], y, w, row_h];
        if intersect(rect, rows_clip).is_none() {
            continue;
        }

        if i == picker.selected {
            let chip = [panel[0] + 4.0 * s, y + 2.0 * s, w - 8.0 * s, row_h - 4.0 * s];
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.accent_soft, rows_clip));
        }
        if let Some(hit) = intersect(rect, rows_clip) {
            out.hit.push(hit, HitRegion::PickerRow(i));
        }

        let baseline = text_baseline(m, y, row_h);
        match row {
            PickerRow::Host { label, presence } => {
                let presence_word = match presence {
                    super::model::TabPresence::Online => "online",
                    super::model::TabPresence::Away => "away",
                    super::model::TabPresence::Unseen => "unseen",
                    super::model::TabPresence::Unreachable => "unreachable",
                };
                let text = format!("{label} — {presence_word}");
                let color = if matches!(presence, super::model::TabPresence::Unreachable) {
                    colors.pill_warn_text
                } else {
                    colors.text_inactive
                };
                out.texts.push(TextRun {
                    text,
                    pos: [panel[0] + PICKER_PAD * s, baseline],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color,
                    clip: rows_clip,
                });
            }
            PickerRow::Session { title, detail, attached, attached_here } => {
                let x = panel[0] + (PICKER_PAD + PICKER_INDENT) * s;
                out.texts.push(TextRun {
                    text: title.clone(),
                    pos: [x, baseline],
                    max_width: w * 0.55,
                    color: colors.text_active,
                    clip: rows_clip,
                });
                // Detail and tags on the right, faint: cwd is orientation,
                // not the headline.
                let tag = if *attached_here {
                    "this window"
                } else if *attached {
                    "attached"
                } else {
                    ""
                };
                let detail = if tag.is_empty() {
                    detail.clone()
                } else if detail.is_empty() {
                    format!("· {tag}")
                } else {
                    format!("{detail} · {tag}")
                };
                let dw = measure(&detail).min(w * 0.4);
                out.texts.push(TextRun {
                    text: detail,
                    pos: [panel[0] + w - PICKER_PAD * s - dw, baseline],
                    max_width: w * 0.4,
                    color: if *attached_here { colors.pill_text } else { colors.text_faint },
                    clip: rows_clip,
                });
            }
            PickerRow::CreateOn { label } => {
                out.texts.push(TextRun {
                    text: format!("+ new session on {label}"),
                    pos: [panel[0] + (PICKER_PAD + PICKER_INDENT) * s, baseline],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color: colors.pill_text,
                    clip: rows_clip,
                });
            }
        }
    }
}

// Shortcuts sheet geometry, logical px. Taller and wider than the picker:
// it is a reference card, not a jump list.
const SHEET_W: f32 = 640.0;
const SHEET_H: f32 = 500.0;
const SHEET_ROW_H: f32 = 28.0;
const SHEET_HEADER_H: f32 = 36.0;
const SHEET_NOTE_H: f32 = 24.0;
const SHEET_GAP: f32 = 8.0;
const CHIP_HPAD: f32 = 8.0;
const CHIP_VPAD: f32 = 3.0;

fn shortcuts_overlay(
    sheet: &super::model::ShortcutsModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    // Same modality recipe as the picker: the scrim swallows what the panel
    // does not, so the grid hears nothing while the sheet is up.
    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::ShortcutsScrim);

    let w = (SHEET_W * s).min(m.width - PICKER_MARGIN * s);
    let h = (SHEET_H * s).min(m.height - PICKER_MARGIN * s);
    let panel = [(m.width - w) / 2.0, (m.height - h) / 2.5, w, h];
    let mut panel_rect = RectInstance::rounded(panel, PICKER_RADIUS * s, colors.panel_bg, no_clip);
    panel_rect.shadow_blur = 24.0 * s;
    panel_rect.shadow_alpha = colors.shadow_alpha;
    out.rects.push(panel_rect);
    // Nothing on the sheet is clickable, but a click on it must not fall
    // through to the scrim and dismiss what the user is reading.
    out.hit.push(panel, HitRegion::ShortcutsPanel);

    let filter_h = m.line_height + 2.0 * PICKER_PAD * s;
    let (filter_text, filter_color) = if sheet.filter.is_empty() {
        ("type to filter shortcuts".to_string(), colors.text_faint)
    } else {
        (sheet.filter.clone(), colors.text_active)
    };
    out.texts.push(TextRun {
        text: filter_text,
        pos: [panel[0] + PICKER_PAD * s, text_baseline(m, panel[1], filter_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color: filter_color,
        clip: panel,
    });
    out.rects.push(RectInstance::filled(
        [panel[0], panel[1] + filter_h, w, HAIRLINE * s],
        colors.line,
        no_clip,
    ));

    let rows_clip =
        [panel[0], panel[1] + filter_h + HAIRLINE * s, w, h - filter_h - HAIRLINE * s];

    let section_h = |section: &super::model::ShortcutSection| {
        SHEET_HEADER_H
            + section.rows.len() as f32 * SHEET_ROW_H
            + if section.note.is_some() { SHEET_NOTE_H } else { 0.0 }
            + SHEET_GAP
    };
    let content_h: f32 = sheet.sections.iter().map(|sec| section_h(sec) * s).sum();
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let scroll = sheet.scroll.clamp(0.0, max_scroll);
    out.shortcuts_scroll = scroll;

    let left = panel[0] + PICKER_PAD * s;
    let right = panel[0] + w - PICKER_PAD * s;
    let mut y = rows_clip[1] - scroll;
    for section in &sheet.sections {
        let header = [panel[0], y, w, SHEET_HEADER_H * s];
        if intersect(header, rows_clip).is_some() {
            out.texts.push(TextRun {
                text: section.title.clone(),
                // Bottom-aligned in its band so the title sits close to its
                // rows rather than the previous section's.
                pos: [left, text_baseline(m, y + (SHEET_HEADER_H - SHEET_ROW_H) * s, SHEET_ROW_H * s)],
                max_width: w - 2.0 * PICKER_PAD * s,
                color: colors.text_faint,
                clip: rows_clip,
            });
        }
        y += SHEET_HEADER_H * s;
        for row in &section.rows {
            let band = [panel[0], y, w, SHEET_ROW_H * s];
            if intersect(band, rows_clip).is_some() {
                let baseline = text_baseline(m, y, SHEET_ROW_H * s);
                out.texts.push(TextRun {
                    text: row.name.clone(),
                    pos: [left, baseline],
                    max_width: w * 0.6,
                    color: colors.text_inactive,
                    clip: rows_clip,
                });
                // The chord, right-aligned in a keycap-look chip.
                let chord_w = measure(&row.chord).min(w * 0.35);
                let chip = [
                    right - chord_w - 2.0 * CHIP_HPAD * s,
                    y + CHIP_VPAD * s,
                    chord_w + 2.0 * CHIP_HPAD * s,
                    SHEET_ROW_H * s - 2.0 * CHIP_VPAD * s,
                ];
                out.rects.push(RectInstance::rounded(
                    chip,
                    RADIUS * s,
                    colors.accent_soft,
                    rows_clip,
                ));
                out.texts.push(TextRun {
                    text: row.chord.clone(),
                    pos: [right - chord_w - CHIP_HPAD * s, baseline],
                    max_width: w * 0.35,
                    color: colors.text_active,
                    clip: rows_clip,
                });
            }
            y += SHEET_ROW_H * s;
        }
        if let Some(note) = &section.note {
            let band = [panel[0], y, w, SHEET_NOTE_H * s];
            if intersect(band, rows_clip).is_some() {
                out.texts.push(TextRun {
                    text: note.clone(),
                    pos: [left, text_baseline(m, y, SHEET_NOTE_H * s)],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color: colors.text_faint,
                    clip: rows_clip,
                });
            }
            y += SHEET_NOTE_H * s;
        }
        y += SHEET_GAP * s;
    }
}

// Settings overlay geometry, logical px. Two-line rows: label + value on the
// first line, description + tags on the second.
const SETTINGS_W: f32 = 720.0;
const SETTINGS_H: f32 = 560.0;
const SETTINGS_ROW_H: f32 = 48.0;
const SETTINGS_HEADER_H: f32 = 38.0;
const SETTINGS_NOTICE_H: f32 = 32.0;
const TOGGLE_W: f32 = 36.0;
const TOGGLE_H: f32 = 20.0;
const TRACK_W: f32 = 120.0;
const TRACK_H: f32 = 4.0;

fn settings_overlay(
    settings: &super::model::SettingsModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
    out: &mut ChromeLayout,
) {
    use super::model::{SettingsRowModel, SettingsValueCell};

    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::SettingsScrim);

    let w = (SETTINGS_W * s).min(m.width - PICKER_MARGIN * s);
    let h = (SETTINGS_H * s).min(m.height - PICKER_MARGIN * s);
    let panel = [(m.width - w) / 2.0, (m.height - h) / 2.5, w, h];
    let mut panel_rect = RectInstance::rounded(panel, PICKER_RADIUS * s, colors.panel_bg, no_clip);
    panel_rect.shadow_blur = 24.0 * s;
    panel_rect.shadow_alpha = colors.shadow_alpha;
    out.rects.push(panel_rect);
    // Swallow panel clicks that miss every row: dismissing what the user is
    // reading because they clicked a header would be hostile.
    out.hit.push(panel, HitRegion::SettingsPanel);

    let filter_h = m.line_height + 2.0 * PICKER_PAD * s;
    let (filter_text, filter_color) = if settings.filter.is_empty() {
        ("type to filter settings".to_string(), colors.text_faint)
    } else {
        (settings.filter.clone(), colors.text_active)
    };
    out.texts.push(TextRun {
        text: filter_text,
        pos: [panel[0] + PICKER_PAD * s, text_baseline(m, panel[1], filter_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color: filter_color,
        clip: panel,
    });
    out.rects.push(RectInstance::filled(
        [panel[0], panel[1] + filter_h, w, HAIRLINE * s],
        colors.line,
        no_clip,
    ));

    let rows_clip =
        [panel[0], panel[1] + filter_h + HAIRLINE * s, w, h - filter_h - HAIRLINE * s];

    let row_h = |row: &SettingsRowModel| match row {
        SettingsRowModel::Group { .. } => SETTINGS_HEADER_H * s,
        SettingsRowModel::Setting { .. } => SETTINGS_ROW_H * s,
        SettingsRowModel::Notice { .. } => SETTINGS_NOTICE_H * s,
    };
    // Row offsets before any drawing, because ensure-visible needs the
    // selected row's extent to decide the scroll it draws with.
    let mut tops = Vec::with_capacity(settings.rows.len());
    let mut content_h = 0.0f32;
    for row in &settings.rows {
        tops.push(content_h);
        content_h += row_h(row);
    }
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let mut scroll = settings.scroll.clamp(0.0, max_scroll);
    if settings.ensure_visible {
        if let (Some(top), Some(row)) =
            (tops.get(settings.selected), settings.rows.get(settings.selected))
        {
            let bottom = top + row_h(row);
            if *top < scroll {
                scroll = *top;
            } else if bottom > scroll + rows_clip[3] {
                scroll = bottom - rows_clip[3];
            }
            scroll = scroll.clamp(0.0, max_scroll);
        }
    }
    out.settings_scroll = scroll;

    let left = panel[0] + PICKER_PAD * s;
    let right = panel[0] + w - PICKER_PAD * s;
    for (i, row) in settings.rows.iter().enumerate() {
        let y = rows_clip[1] + tops[i] - scroll;
        let band = [panel[0], y, w, row_h(row)];
        let Some(visible) = intersect(band, rows_clip) else { continue };

        match row {
            SettingsRowModel::Notice { text } => {
                // A warn-tinted band across the panel: pinned truth, not a row.
                let band_rect = [panel[0] + 4.0 * s, y + 2.0 * s, w - 8.0 * s, band[3] - 4.0 * s];
                out.rects.push(RectInstance::rounded(
                    band_rect,
                    RADIUS * s,
                    colors.pill_warn_bg,
                    rows_clip,
                ));
                out.texts.push(TextRun {
                    text: text.clone(),
                    pos: [left, text_baseline(m, y, band[3])],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color: colors.pill_warn_text,
                    clip: rows_clip,
                });
            }
            SettingsRowModel::Group { title } => {
                out.texts.push(TextRun {
                    text: title.clone(),
                    pos: [
                        left,
                        text_baseline(
                            m,
                            y + (SETTINGS_HEADER_H - 24.0) * s,
                            24.0 * s,
                        ),
                    ],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color: colors.text_faint,
                    clip: rows_clip,
                });
            }
            SettingsRowModel::Setting {
                label,
                key,
                description,
                value,
                provenance,
                restart,
                inert,
                modified,
            } => {
                if i == settings.selected {
                    let chip =
                        [panel[0] + 4.0 * s, y + 2.0 * s, w - 8.0 * s, band[3] - 4.0 * s];
                    out.rects.push(RectInstance::rounded(
                        chip,
                        RADIUS * s,
                        colors.accent_soft,
                        rows_clip,
                    ));
                }
                out.hit.push(visible, HitRegion::SettingsRow(i));

                let line1_h = band[3] / 2.0;
                let baseline1 = text_baseline(m, y, line1_h);
                let baseline2 = text_baseline(m, y + line1_h, line1_h);

                // The modified dot: a small accent square beside the label.
                // Text markers survive what colour alone does not, but this
                // one pairs with reset-to-default later; keep it visual.
                if *modified {
                    let dot = [left, y + (line1_h - 6.0 * s) / 2.0, 6.0 * s, 6.0 * s];
                    out.rects.push(RectInstance::rounded(dot, 3.0 * s, colors.accent, rows_clip));
                }
                let label_x = left + 14.0 * s;
                out.texts.push(TextRun {
                    text: label.clone(),
                    pos: [label_x, baseline1],
                    max_width: w * 0.4,
                    color: colors.text_active,
                    clip: rows_clip,
                });

                // Second line: description left, tags right. The dotted key
                // rides with the description so the user can grep their
                // config for exactly what this row is.
                let desc = if description.is_empty() {
                    key.clone()
                } else {
                    format!("{key} — {description}")
                };
                out.texts.push(TextRun {
                    text: desc,
                    pos: [label_x, baseline2],
                    max_width: w * 0.62,
                    color: colors.text_faint,
                    clip: rows_clip,
                });

                let mut tag_x = right;
                let mut push_tag = |text: String, color, tag_x: &mut f32| {
                    let tw = measure(&text).min(w * 0.35);
                    *tag_x -= tw;
                    out.texts.push(TextRun {
                        text,
                        pos: [*tag_x, baseline2],
                        max_width: w * 0.35,
                        color,
                        clip: rows_clip,
                    });
                    *tag_x -= 12.0 * s;
                };
                if *inert {
                    push_tag("not applied yet".to_string(), colors.text_faint, &mut tag_x);
                }
                if *restart {
                    push_tag("applies on next launch".to_string(), colors.text_faint, &mut tag_x);
                }
                if let Some((text, warn)) = provenance {
                    let color = if *warn { colors.pill_warn_text } else { colors.text_faint };
                    push_tag(text.clone(), color, &mut tag_x);
                }

                // The value cell, right-aligned on the first line.
                match value {
                    SettingsValueCell::Toggle { on } => {
                        let track = [
                            right - TOGGLE_W * s,
                            y + (line1_h - TOGGLE_H * s) / 2.0,
                            TOGGLE_W * s,
                            TOGGLE_H * s,
                        ];
                        let fill = if *on { colors.accent } else { colors.line };
                        out.rects.push(RectInstance::rounded(
                            track,
                            TOGGLE_H * s / 2.0,
                            fill,
                            rows_clip,
                        ));
                        let knob_d = (TOGGLE_H - 4.0) * s;
                        let knob_x = if *on {
                            track[0] + track[2] - knob_d - 2.0 * s
                        } else {
                            track[0] + 2.0 * s
                        };
                        out.rects.push(RectInstance::rounded(
                            [knob_x, track[1] + 2.0 * s, knob_d, knob_d],
                            knob_d / 2.0,
                            colors.text_active,
                            rows_clip,
                        ));
                        // Pushed after the row, so the track outranks it: a
                        // click here flips, a click elsewhere only selects.
                        if let Some(hit) = intersect(track, rows_clip) {
                            out.hit.push(hit, HitRegion::SettingsToggle(i));
                        }
                    }
                    SettingsValueCell::Select { value } => {
                        let vw = measure(value).min(w * 0.3);
                        out.texts.push(TextRun {
                            text: value.clone(),
                            pos: [right - vw, baseline1],
                            max_width: w * 0.3,
                            color: colors.text_active,
                            clip: rows_clip,
                        });
                    }
                    SettingsValueCell::Slider { frac, text } => {
                        let track = [
                            right - TRACK_W * s,
                            y + (line1_h - TRACK_H * s) / 2.0,
                            TRACK_W * s,
                            TRACK_H * s,
                        ];
                        out.rects.push(RectInstance::rounded(
                            track,
                            TRACK_H * s / 2.0,
                            colors.line,
                            rows_clip,
                        ));
                        out.rects.push(RectInstance::rounded(
                            [track[0], track[1], track[2] * frac.clamp(0.0, 1.0), track[3]],
                            TRACK_H * s / 2.0,
                            colors.accent,
                            rows_clip,
                        ));
                        let tw = measure(text).min(w * 0.15);
                        out.texts.push(TextRun {
                            text: text.clone(),
                            pos: [track[0] - tw - 8.0 * s, baseline1],
                            max_width: w * 0.15,
                            color: colors.text_active,
                            clip: rows_clip,
                        });
                    }
                    SettingsValueCell::Text { text } => {
                        let vw = measure(text).min(w * 0.35);
                        out.texts.push(TextRun {
                            text: text.clone(),
                            pos: [right - vw, baseline1],
                            max_width: w * 0.35,
                            color: colors.text_active,
                            clip: rows_clip,
                        });
                    }
                    SettingsValueCell::ReadOnly { text } => {
                        let vw = measure(text).min(w * 0.35);
                        out.texts.push(TextRun {
                            text: text.clone(),
                            pos: [right - vw, baseline1],
                            max_width: w * 0.35,
                            color: colors.text_faint,
                            clip: rows_clip,
                        });
                    }
                    SettingsValueCell::Editing { buffer, error } => {
                        // The caret is a character, not a rect: it inherits
                        // the text clip and colour for free, and this is not
                        // a text editor — there is no selection to draw.
                        let text = format!("{buffer}▏");
                        let vw = measure(&text).min(w * 0.35);
                        let color =
                            if *error { colors.pill_warn_text } else { colors.text_active };
                        out.texts.push(TextRun {
                            text,
                            pos: [right - vw, baseline1],
                            max_width: w * 0.35,
                            color,
                            clip: rows_clip,
                        });
                    }
                }
            }
        }
    }
}

/// The origin/presence pill's label, or `None` for a plain local tab.
///
/// Presence is *words*, not colour alone: "acting on the wrong machine" is
/// the mistake this UI exists to prevent, and "which machine" must survive
/// any theme.
fn pill_label(tab: &TabModel) -> Option<(String, bool)> {
    match (&tab.origin, tab.presence) {
        (TabOrigin::Local, _) => None,
        (TabOrigin::Remote { host_label }, TabPresence::Unreachable) => {
            Some((format!("{host_label} · unreachable"), true))
        }
        (TabOrigin::Remote { host_label }, TabPresence::Away) => {
            Some((format!("{host_label} · away"), false))
        }
        (TabOrigin::Remote { host_label }, _) => Some((host_label.clone(), false)),
    }
}

fn intersect(r: [f32; 4], c: [f32; 4]) -> Option<[f32; 4]> {
    let x0 = r[0].max(c[0]);
    let y0 = r[1].max(c[1]);
    let x1 = (r[0] + r[2]).min(c[0] + c[2]);
    let y1 = (r[1] + r[3]).min(c[1] + c[3]);
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

fn horizontal(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
) -> ChromeLayout {
    let s = m.scale;
    let mut out = ChromeLayout::default();
    let no_clip = [0.0, 0.0, m.width, m.height];

    let sh = m.strip_height * s;
    let strip = [0.0, 0.0, m.width, sh];
    out.rects.push(RectInstance::filled(strip, colors.strip_bg, no_clip));
    out.rects.push(RectInstance::filled(
        [0.0, sh - HAIRLINE * s, m.width, HAIRLINE * s],
        colors.line,
        no_clip,
    ));
    out.hit.push(strip, HitRegion::Strip);

    // The traffic lights are native controls that swallow their own clicks;
    // the reserve keeps tabs from being drawn *under* them, and behaves as
    // window drag like the rest of the empty strip.
    let reserve = model.traffic_inset.map_or(0.0, |t| t[0]) + EDGE_PAD * s;
    out.hit.push([0.0, 0.0, reserve, sh], HitRegion::Drag);

    let avail = (m.width - reserve - EDGE_PAD * s).max(0.0);
    let clip = [reserve, 0.0, avail, sh];

    let n = model.tabs.len();
    let tab_w = if n == 0 {
        0.0
    } else {
        (avail / n as f32).clamp(TAB_MIN * s, TAB_MAX * s)
    };
    let new_tab_w = NEW_TAB * s;
    let content_w = n as f32 * tab_w + new_tab_w;
    let max_scroll = (content_w - avail).max(0.0);
    out.strip_scroll = model.strip_scroll.clamp(0.0, max_scroll);

    for (i, tab) in model.tabs.iter().enumerate() {
        let x = reserve + i as f32 * tab_w - out.strip_scroll;
        let slot = [x, 0.0, tab_w, sh];
        let chip = [x + TAB_HPAD * s, TAB_VPAD * s, tab_w - 2.0 * TAB_HPAD * s, sh - 2.0 * TAB_VPAD * s];

        let active = i == model.active;
        let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
        if active {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_active_bg, clip));
        } else if hovered {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_hover_bg, clip));
        }
        if let Some(hit) = intersect(slot, clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        // Right-to-left within the tab: close button, then the origin pill,
        // and the title gets whatever is left.
        let mut right = x + tab_w - TEXT_PAD * s;

        if active || hovered {
            let close = [right - CLOSE * s, (sh - CLOSE * s) / 2.0, CLOSE * s, CLOSE * s];
            if let Some(hit) = intersect(close, clip) {
                out.hit.push(hit, HitRegion::TabClose(tab.addr));
            }
            let glyph_w = measure("×");
            out.texts.push(TextRun {
                text: "×".into(),
                pos: [close[0] + (close[2] - glyph_w) / 2.0, text_baseline(m, 0.0, sh)],
                max_width: close[2],
                color: colors.text_inactive,
                clip,
            });
            right = close[0] - TEXT_PAD * s / 2.0;
        }

        if let Some((label, warn)) = pill_label(tab) {
            let text_w = measure(&label);
            // The pill may take up to half the tab; past that the label
            // truncates rather than squeezing the title out entirely.
            let pill_w = (text_w + 2.0 * PILL_HPAD * s).min(tab_w * 0.5);
            let pill_h = (m.line_height + 2.0 * s).min(sh - 2.0 * TAB_VPAD * s);
            let pill = [right - pill_w, (sh - pill_h) / 2.0, pill_w, pill_h];
            let (bg, fg) = if warn {
                (colors.pill_warn_bg, colors.pill_warn_text)
            } else {
                (colors.pill_bg, colors.pill_text)
            };
            out.rects.push(RectInstance::rounded(pill, pill_h / 2.0, bg, clip));
            out.texts.push(TextRun {
                text: label,
                pos: [pill[0] + PILL_HPAD * s, text_baseline(m, 0.0, sh)],
                max_width: pill_w - 2.0 * PILL_HPAD * s,
                color: fg,
                clip,
            });
            right = pill[0] - TEXT_PAD * s / 2.0;
        }

        let text_x = x + TEXT_PAD * s;
        let title_color = match (active, model.focused, tab.connecting) {
            (_, _, true) => colors.text_faint,
            (true, true, _) => colors.text_active,
            _ => colors.text_inactive,
        };
        out.texts.push(TextRun {
            text: tab.title.clone(),
            pos: [text_x, text_baseline(m, 0.0, sh)],
            max_width: (right - text_x).max(0.0),
            color: title_color,
            clip,
        });
    }

    // The new-tab button trails the last tab and scrolls with the content.
    let nt_x = reserve + n as f32 * tab_w - out.strip_scroll;
    let nt = [nt_x + 2.0 * s, (sh - NEW_TAB * s) / 2.0, NEW_TAB * s, NEW_TAB * s];
    if model.hover == Some(HitRegion::NewTab) {
        out.rects.push(RectInstance::rounded(nt, RADIUS * s, colors.tab_hover_bg, clip));
    }
    if let Some(hit) = intersect(nt, clip) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    let plus_w = measure("+");
    out.texts.push(TextRun {
        text: "+".into(),
        pos: [nt[0] + (nt[2] - plus_w) / 2.0, text_baseline(m, 0.0, sh)],
        max_width: nt[2],
        color: colors.text_inactive,
        clip,
    });

    // Whatever the content does not cover is a drag handle, like any titlebar.
    let drag_from = (nt[0] + nt[2] + 2.0 * s).min(m.width);
    if drag_from < m.width {
        out.hit.push([drag_from, 0.0, m.width - drag_from, sh], HitRegion::Drag);
    }

    out
}

fn vertical(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
) -> ChromeLayout {
    let s = m.scale;
    let mut out = ChromeLayout::default();
    let no_clip = [0.0, 0.0, m.width, m.height];

    let sw = m.sidebar_width * s;
    let sidebar = [0.0, 0.0, sw, m.height];
    out.rects.push(RectInstance::filled(sidebar, colors.strip_bg, no_clip));
    out.rects.push(RectInstance::filled(
        [sw - HAIRLINE * s, 0.0, HAIRLINE * s, m.height],
        colors.line,
        no_clip,
    ));
    out.hit.push(sidebar, HitRegion::Strip);

    // The header band exists so the traffic lights have chrome under them
    // and the window keeps a place to grab; the sidebar being wider than the
    // button cluster is what lets the grid run full height beside it.
    let header_h = model.traffic_inset.map_or(HEADER_MIN * s, |t| t[1].max(HEADER_MIN * s));
    out.hit.push([0.0, 0.0, sw, header_h], HitRegion::Drag);

    let rows_clip = [0.0, header_h, sw, (m.height - header_h).max(0.0)];
    let row_h = ROW_H * s;
    let n = model.tabs.len();
    let content_h = (n as f32 + 1.0) * row_h; // + the new-tab row
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    out.strip_scroll = model.strip_scroll.clamp(0.0, max_scroll);

    for (i, tab) in model.tabs.iter().enumerate() {
        let y = header_h + i as f32 * row_h - out.strip_scroll;
        let row = [0.0, y, sw, row_h];
        let chip = [ROW_HPAD * s, y + 2.0 * s, sw - 2.0 * ROW_HPAD * s, row_h - 4.0 * s];

        let active = i == model.active;
        let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
        if active {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_active_bg, rows_clip));
        } else if hovered {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_hover_bg, rows_clip));
        }
        if let Some(hit) = intersect(row, rows_clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        let mut right = sw - (ROW_HPAD + TEXT_PAD) * s;
        if active || hovered {
            let close = [right - CLOSE * s, y + (row_h - CLOSE * s) / 2.0, CLOSE * s, CLOSE * s];
            if let Some(hit) = intersect(close, rows_clip) {
                out.hit.push(hit, HitRegion::TabClose(tab.addr));
            }
            let glyph_w = measure("×");
            out.texts.push(TextRun {
                text: "×".into(),
                pos: [
                    close[0] + (close[2] - glyph_w) / 2.0,
                    text_baseline(m, y, row_h),
                ],
                max_width: close[2],
                color: colors.text_inactive,
                clip: rows_clip,
            });
            right = close[0] - TEXT_PAD * s / 2.0;
        }

        let text_x = (ROW_HPAD + TEXT_PAD) * s;
        let title_color = match (active, model.focused, tab.connecting) {
            (_, _, true) => colors.text_faint,
            (true, true, _) => colors.text_active,
            _ => colors.text_inactive,
        };

        // The sidebar is the loud fleet view: every remote tab gets a second
        // line naming its machine, in words.
        if let Some((label, warn)) = pill_label(tab) {
            let block = 2.0 * m.line_height + LINE_GAP * s;
            let top = y + (row_h - block).max(0.0) / 2.0;
            out.texts.push(TextRun {
                text: tab.title.clone(),
                pos: [text_x, top + m.baseline],
                max_width: (right - text_x).max(0.0),
                color: title_color,
                clip: rows_clip,
            });
            out.texts.push(TextRun {
                text: label,
                pos: [text_x, top + m.line_height + LINE_GAP * s + m.baseline],
                max_width: (right - text_x).max(0.0),
                color: if warn { colors.pill_warn_text } else { colors.text_faint },
                clip: rows_clip,
            });
        } else {
            out.texts.push(TextRun {
                text: tab.title.clone(),
                pos: [text_x, text_baseline(m, y, row_h)],
                max_width: (right - text_x).max(0.0),
                color: title_color,
                clip: rows_clip,
            });
        }
    }

    // New-tab row.
    let y = header_h + n as f32 * row_h - out.strip_scroll;
    let row = [0.0, y, sw, row_h];
    if model.hover == Some(HitRegion::NewTab) {
        let chip = [ROW_HPAD * s, y + 2.0 * s, sw - 2.0 * ROW_HPAD * s, row_h - 4.0 * s];
        out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_hover_bg, rows_clip));
    }
    if let Some(hit) = intersect(row, rows_clip) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    out.texts.push(TextRun {
        text: "+".into(),
        pos: [(ROW_HPAD + TEXT_PAD) * s, text_baseline(m, y, row_h)],
        max_width: sw,
        color: colors.text_inactive,
        clip: rows_clip,
    });

    out
}

/// Baseline for one line of text vertically centred in a band starting at
/// `y` with height `h`.
fn text_baseline(m: &ChromeMetrics, y: f32, h: f32) -> f32 {
    y + (h - m.line_height).max(0.0) / 2.0 + m.baseline
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_proto::{HostId, SessionAddr, SessionId};

    fn addr(n: u8) -> SessionAddr {
        SessionAddr::new(HostId::from_bytes([n; 32]), SessionId(u64::from(n)))
    }

    fn tab(n: u8, origin: TabOrigin, presence: TabPresence) -> TabModel {
        TabModel {
            addr: addr(n),
            title: format!("tab {n}"),
            origin,
            presence,
            connecting: false,
        }
    }

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0)
    }

    fn metrics(width: f32, height: f32, scale: f32) -> ChromeMetrics {
        ChromeMetrics {
            width,
            height,
            scale,
            strip_height: 38.0,
            sidebar_width: 220.0,
            line_height: 20.0 * scale,
            baseline: 15.0 * scale,
        }
    }

    fn model(tabs: Vec<TabModel>, position: TabsPosition) -> ChromeModel {
        ChromeModel {
            tabs,
            active: 0,
            position,
            strip_scroll: 0.0,
            hover: None,
            traffic_inset: None,
            focused: true,
            picker: None,
            shortcuts: None,
            settings: None,
        }
    }

    /// Eight pixels a character: enough for the tests to reason about
    /// truncation without a font.
    fn measure(s: &str) -> f32 {
        s.chars().count() as f32 * 8.0
    }

    #[test]
    fn every_drawn_tab_is_hit_at_its_centre() {
        // The load-bearing property of the whole module: the hit map and the
        // visuals come from one pass, so the middle of what you see is what
        // you click.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let tabs = vec![
                tab(1, TabOrigin::Local, TabPresence::Online),
                tab(2, TabOrigin::Remote { host_label: "alien".into() }, TabPresence::Online),
                tab(3, TabOrigin::Local, TabPresence::Online),
            ];
            let m = metrics(1200.0, 800.0, 1.0);
            let l = layout(&model(tabs, position), &colors(), &m, &mut measure);
            for n in 1..=3u8 {
                let found = (0..1200).step_by(2).find_map(|x| {
                    (0..800).step_by(2).find_map(|y| {
                        (l.hit.hit(x as f32, y as f32) == Some(HitRegion::Tab(addr(n))))
                            .then_some(())
                    })
                });
                assert!(found.is_some(), "tab {n} must be clickable somewhere ({position:?})");
            }
        }
    }

    #[test]
    fn the_close_button_wins_over_its_own_tab() {
        // TabClose is pushed after Tab, and the reverse-order lookup is what
        // makes that ordering meaningful. If this fails, close buttons are
        // decoration.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        let close = (0..1200).find_map(|x| {
            (0..40).find_map(|y| {
                (l.hit.hit(x as f32, y as f32) == Some(HitRegion::TabClose(addr(1))))
                    .then_some((x, y))
            })
        });
        assert!(close.is_some(), "the active tab must expose a close button");
    }

    #[test]
    fn only_the_active_or_hovered_tab_offers_close() {
        let tabs = vec![
            tab(1, TabOrigin::Local, TabPresence::Online),
            tab(2, TabOrigin::Local, TabPresence::Online),
        ];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        let closes: Vec<_> = (0..1200)
            .flat_map(|x| (0..40).map(move |y| (x, y)))
            .filter_map(|(x, y)| match l.hit.hit(x as f32, y as f32) {
                Some(HitRegion::TabClose(a)) => Some(a),
                _ => None,
            })
            .collect();
        assert!(closes.iter().all(|a| *a == addr(1)), "only the active tab closes");
        assert!(!closes.is_empty());
    }

    #[test]
    fn overflow_scroll_is_clamped_and_tabs_clip_to_the_strip() {
        // Twenty tabs at minimum width overflow a 1000px strip; the layout
        // must clamp a wild scroll value and keep every emitted hit region
        // inside the strip, or invisible tabs become clickable.
        let tabs: Vec<_> =
            (1..=20).map(|n| tab(n, TabOrigin::Local, TabPresence::Online)).collect();
        let m = metrics(1000.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.strip_scroll = 1e9;
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.strip_scroll > 0.0, "an overflowing strip scrolls");
        assert!(l.strip_scroll < 1e9, "and the scroll is clamped to the content");
        // With the scroll pinned at max, the first tab is far off-screen and
        // must not be hittable anywhere.
        let anywhere = (0..1000).step_by(2).find(|&x| {
            (0..40).any(|y| l.hit.hit(x as f32, y as f32) == Some(HitRegion::Tab(addr(1))))
        });
        assert!(anywhere.is_none(), "a scrolled-out tab must not answer clicks");
    }

    #[test]
    fn the_traffic_light_reserve_is_drag_not_tabs() {
        // Tabs drawn under the native buttons would be unclickable pixels;
        // the reserve keeps them out and stays a drag handle.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 2.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.traffic_inset = Some([140.0, 56.0]);
        let l = layout(&mo, &colors(), &m, &mut measure);
        for x in [5.0, 70.0, 139.0] {
            assert_eq!(
                l.hit.hit(x, 10.0),
                Some(HitRegion::Drag),
                "the button cluster's zone drags the window"
            );
        }
    }

    #[test]
    fn the_sidebar_header_drags_and_rows_start_below_it() {
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Left);
        mo.traffic_inset = Some([70.0, 40.0]);
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert_eq!(l.hit.hit(30.0, 20.0), Some(HitRegion::Drag), "header band drags");
        assert_eq!(
            l.hit.hit(30.0, 40.0 + 22.0),
            Some(HitRegion::Tab(addr(1))),
            "the first row sits directly below the header"
        );
    }

    #[test]
    fn a_remote_tab_names_its_machine_in_words() {
        // Colour is not enough on its own (#23). The label must appear as
        // text in both orientations, and an unreachable host must say so.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let tabs = vec![tab(
                2,
                TabOrigin::Remote { host_label: "alien".into() },
                TabPresence::Unreachable,
            )];
            let m = metrics(1200.0, 800.0, 1.0);
            let l = layout(&model(tabs, position), &colors(), &m, &mut measure);
            assert!(
                l.texts.iter().any(|t| t.text.contains("alien") && t.text.contains("unreachable")),
                "{position:?} must spell out host and unreachability"
            );
        }
    }

    #[test]
    fn the_new_tab_button_exists_in_both_orientations() {
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
            let m = metrics(1200.0, 800.0, 1.0);
            let l = layout(&model(tabs, position), &colors(), &m, &mut measure);
            let found = (0..1200).step_by(2).find_map(|x| {
                (0..800).step_by(2).find_map(|y| {
                    (l.hit.hit(x as f32, y as f32) == Some(HitRegion::NewTab)).then_some(())
                })
            });
            assert!(found.is_some(), "no way to open a tab in {position:?}");
        }
    }

    #[test]
    fn the_picker_sits_above_everything_and_its_scrim_catches_the_rest() {
        // Modal means modal: a click on a row is that row, a click anywhere
        // else while the picker is open must never reach a tab or the grid.
        use crate::chrome::model::{PickerModel, PickerRow};
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.picker = Some(PickerModel {
            rows: vec![
                PickerRow::Host { label: "andy-mac".into(), presence: TabPresence::Online },
                PickerRow::Session {
                    title: "vim".into(),
                    detail: "~/dev".into(),
                    attached: false,
                    attached_here: false,
                },
                PickerRow::CreateOn { label: "andy-mac".into() },
            ],
            selected: 1,
            filter: String::new(),
            scroll: 0.0,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);

        let mut seen_rows = std::collections::HashSet::new();
        let mut scrim_hits = 0u32;
        for x in (0..1200).step_by(4) {
            for y in (0..800).step_by(4) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::PickerRow(i)) => {
                        seen_rows.insert(i);
                    }
                    Some(HitRegion::PickerScrim) => scrim_hits += 1,
                    Some(other) => {
                        panic!("a click at ({x},{y}) escaped the picker: {other:?}")
                    }
                    None => panic!("({x},{y}) hit nothing; the scrim must cover the window"),
                }
            }
        }
        assert_eq!(seen_rows, [0usize, 1, 2].into(), "every row must be clickable");
        assert!(scrim_hits > 0, "the scrim must be reachable around the panel");
    }

    #[test]
    fn the_shortcuts_sheet_is_modal_like_the_picker() {
        // Same definition of modal as the picker test above: every point in
        // the window answers as the sheet's panel or its scrim, and a click
        // can never reach a tab or fall through to the grid while it is up.
        use crate::chrome::model::{ShortcutRow, ShortcutSection, ShortcutsModel};
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.shortcuts = Some(ShortcutsModel {
            sections: vec![ShortcutSection {
                title: "Tabs".into(),
                rows: vec![
                    ShortcutRow { name: "New tab".into(), chord: "⌘T".into() },
                    ShortcutRow { name: "Close tab".into(), chord: "⌘W".into() },
                ],
                note: Some("a note".into()),
            }],
            filter: String::new(),
            scroll: 0.0,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);

        let mut panel_hits = 0u32;
        let mut scrim_hits = 0u32;
        for x in (0..1200).step_by(4) {
            for y in (0..800).step_by(4) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::ShortcutsPanel) => panel_hits += 1,
                    Some(HitRegion::ShortcutsScrim) => scrim_hits += 1,
                    Some(other) => panic!("a click at ({x},{y}) escaped the sheet: {other:?}"),
                    None => panic!("({x},{y}) hit nothing; the scrim must cover the window"),
                }
            }
        }
        assert!(panel_hits > 0, "the panel must swallow clicks on itself");
        assert!(scrim_hits > 0, "the scrim must be reachable around the panel");
    }

    #[test]
    fn a_long_sheet_clamps_its_scroll_to_the_content() {
        // Forty rows overflow a 500-logical-px panel; a wild scroll value
        // must clamp to the content or the list scrolls into blank space
        // and appears empty.
        use crate::chrome::model::{ShortcutRow, ShortcutSection, ShortcutsModel};
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(Vec::new(), TabsPosition::Top);
        mo.shortcuts = Some(ShortcutsModel {
            sections: vec![ShortcutSection {
                title: "Everything".into(),
                rows: (0..40)
                    .map(|i| ShortcutRow { name: format!("row {i}"), chord: "⌘X".into() })
                    .collect(),
                note: None,
            }],
            filter: String::new(),
            scroll: 1e9,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.shortcuts_scroll > 0.0, "an overflowing sheet scrolls");
        assert!(l.shortcuts_scroll < 1e9, "and the scroll is clamped to the content");
    }

    fn settings_rows(n: usize) -> Vec<crate::chrome::model::SettingsRowModel> {
        use crate::chrome::model::{SettingsRowModel, SettingsValueCell};
        let mut rows = vec![SettingsRowModel::Group { title: "Text".into() }];
        rows.extend((0..n).map(|i| SettingsRowModel::Setting {
            label: format!("Setting {i}"),
            key: format!("group.key_{i}"),
            description: "a setting".into(),
            value: SettingsValueCell::Toggle { on: i % 2 == 0 },
            provenance: None,
            restart: false,
            inert: false,
            modified: false,
        }));
        rows
    }

    #[test]
    fn the_settings_overlay_is_modal_like_the_picker() {
        // The same definition of modal the picker and the sheet answer to:
        // every point resolves to a settings region, nothing falls through.
        use crate::chrome::model::SettingsModel;
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.settings = Some(SettingsModel {
            rows: settings_rows(3),
            selected: 1,
            filter: String::new(),
            scroll: 0.0,
            ensure_visible: false,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);

        let mut seen_rows = std::collections::HashSet::new();
        let mut scrim_hits = 0u32;
        for x in (0..1200).step_by(4) {
            for y in (0..800).step_by(4) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::SettingsRow(i) | HitRegion::SettingsToggle(i)) => {
                        seen_rows.insert(i);
                    }
                    Some(HitRegion::SettingsPanel) => {}
                    Some(HitRegion::SettingsScrim) => scrim_hits += 1,
                    Some(other) => {
                        panic!("a click at ({x},{y}) escaped the settings overlay: {other:?}")
                    }
                    None => panic!("({x},{y}) hit nothing; the scrim must cover the window"),
                }
            }
        }
        assert_eq!(
            seen_rows,
            [1usize, 2, 3].into(),
            "every setting row must be clickable; the header (row 0) must not be"
        );
        assert!(scrim_hits > 0, "the scrim must be reachable around the panel");
    }

    #[test]
    fn keyboard_navigation_never_acts_on_an_offscreen_row() {
        // Forty rows overflow the panel. With the selection at the end and
        // the scroll at the top, ensure_visible must move the scroll so the
        // selected row is actually hittable — otherwise arrows act on rows
        // the user cannot see.
        use crate::chrome::model::SettingsModel;
        let m = metrics(1200.0, 800.0, 1.0);
        let rows = settings_rows(40);
        let selected = rows.len() - 1;
        let mut mo = model(Vec::new(), TabsPosition::Top);
        mo.settings = Some(SettingsModel {
            rows,
            selected,
            filter: String::new(),
            scroll: 0.0,
            ensure_visible: true,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.settings_scroll > 0.0, "the view must have moved to the selection");
        let found = (0..1200).step_by(4).any(|x| {
            (0..800).step_by(4).any(|y| {
                l.hit.hit(x as f32, y as f32) == Some(HitRegion::SettingsRow(selected))
            })
        });
        assert!(found, "the selected row must be visible and hittable after ensure_visible");

        // And the wheel must stay free: without the flag the scroll stays
        // where the user put it, selection offscreen or not.
        if let Some(settings) = mo.settings.as_mut() {
            settings.ensure_visible = false;
            settings.scroll = 0.0;
        }
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            l.settings_scroll.abs() < f32::EPSILON,
            "without ensure_visible the scroll must not snap to the selection"
        );
    }

    #[test]
    fn an_empty_model_still_produces_chrome() {
        // Zero tabs happens transiently while the last tab closes; layout
        // must not panic and the strip must still exist.
        let m = metrics(800.0, 600.0, 1.0);
        let l = layout(&model(Vec::new(), TabsPosition::Top), &colors(), &m, &mut measure);
        assert!(!l.hit.is_empty());
        assert!(!l.rects.is_empty());
    }
}
