//! The Settings tab's screen (design §11): a 214px category rail and a
//! content column, drawn over the grid area while the active tab is
//! Settings. Sibling of `screens.rs`, same discipline: pure, event-driven,
//! rects + text runs + hit regions out of one pass.
//!
//! The overlay this replaces owned only the *container*; the machinery it
//! rented — `settings_ui::build_*`, adjust/parse/slider quantisation — is
//! reused whole. What is new here is the §11 geometry: the rail, the
//! per-category content column, the responsive row wrap, and the widget
//! vocabulary's chrome.

use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::HitRegion;
use super::layout::{ChromeLayout, TextRun};
use super::model::{ChromeMetrics, SettingsRowModel, SettingsScreenModel, SettingsValueCell};
use super::theme::ChromeColors;

// §11 geometry, logical px. The values are the design handoff's
// (docs/design/client-ui/README.md §11) — change them there first or not
// at all.
pub const RAIL_W: f32 = 214.0;
const RAIL_PAD: f32 = 12.0;
const FILTER_H: f32 = 30.0;
const CAT_ROW_H: f32 = 30.0;
const CAT_GAP: f32 = 2.0;
const CONTENT_X: f32 = 30.0;
const HEADER_PAD_TOP: f32 = 22.0;
const HEADER_PAD_BOTTOM: f32 = 16.0;
const HEADING_PX: f32 = 17.0;
const LEDE_PX: f32 = 12.0;
const ROW_VPAD: f32 = 16.0;
/// Control column width (§11: `flex: 0 1 262px`).
const CONTROL_W: f32 = 262.0;
/// Below this content-column width the control drops to its own line,
/// right-aligned, instead of crushing the label column (§11).
pub const WRAP_AT: f32 = 400.0;
const FOOTER_H: f32 = 42.0;
const DESC_PX: f32 = 11.5;
const KEY_PX: f32 = 10.5;
const LABEL_PX: f32 = 13.0;
const CHIP_PX: f32 = 10.0;
const LINE_H: f32 = 17.0;
const HAIRLINE: f32 = 1.0;
// Widgets (§11's table).
const TOGGLE_W: f32 = 38.0;
const TOGGLE_H: f32 = 22.0;
const TRACK_W: f32 = 150.0;
const TRACK_H: f32 = 4.0;
const SLIDER_VALUE_W: f32 = 44.0;
const STEP_H: f32 = 30.0;
const STEP_BTN: f32 = 30.0;
const SEG_H: f32 = 28.0;
const SELECT_W: f32 = 180.0;
const SELECT_H: f32 = 32.0;
const LIST_ROW_H: f32 = 30.0;
const LIST_GAP: f32 = 4.0;
const CHIP_H: f32 = 26.0;
const MENU_W: f32 = 288.0;
const MENU_RADIUS: f32 = 11.0;

fn baseline_in(y: f32, h: f32, px: f32) -> f32 {
    y + (h + px * 0.72) / 2.0
}

/// Greedy word wrap against the injected measure — the §11 descriptions and
/// ledes are sentences, and an ellipsis mid-sentence loses the clause that
/// carries the meaning.
fn wrap_text(
    text: &str,
    px: f32,
    max_w: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if !line.is_empty() && measure(&candidate, px, false, 0.0) > max_w {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// A dashed 1px border for the add affordances — same placeholder-grade
/// recipe as the fleet's asleep cards (`screens::dashed_border` is private
/// to its own concerns; four dashes of arithmetic beat a wider interface).
fn dashed_border(
    rects: &mut Vec<RectInstance>,
    rect: [f32; 4],
    s: f32,
    color: LinearRgba,
    clip: [f32; 4],
) {
    let dash = 4.0 * s;
    let t = HAIRLINE * s;
    let mut x = rect[0] + dash;
    while x + dash < rect[0] + rect[2] - dash {
        rects.push(RectInstance::filled([x, rect[1], dash, t], color, clip));
        rects.push(RectInstance::filled([x, rect[1] + rect[3] - t, dash, t], color, clip));
        x += 2.0 * dash;
    }
    let mut y = rect[1] + dash;
    while y + dash < rect[1] + rect[3] - dash {
        rects.push(RectInstance::filled([rect[0], y, t, dash], color, clip));
        rects.push(RectInstance::filled([rect[0] + rect[2] - t, y, t, dash], color, clip));
        y += 2.0 * dash;
    }
}

/// The height of one row's control, in logical px — the row layout and the
/// scroll math must agree on it, so it is computed once, here.
fn control_height(cell: &SettingsValueCell) -> f32 {
    match cell {
        SettingsValueCell::Toggle { .. } => TOGGLE_H,
        SettingsValueCell::Segmented { .. } => SEG_H,
        SettingsValueCell::Select { .. } => SELECT_H,
        SettingsValueCell::Slider { .. } => 14.0,
        SettingsValueCell::Stepper { .. } => STEP_H,
        SettingsValueCell::Text { .. }
        | SettingsValueCell::ReadOnly { .. }
        | SettingsValueCell::Editing { .. } => 18.0,
        SettingsValueCell::FontList { faces } => {
            (faces.len() + 1) as f32 * (LIST_ROW_H + LIST_GAP)
        }
        SettingsValueCell::TagList { .. } => CHIP_H,
        SettingsValueCell::KeyValue { entries } => {
            (entries.len() + 1) as f32 * (LIST_ROW_H + LIST_GAP)
        }
    }
}

/// One row's extents: (total height, wrapped description lines). `narrow`
/// stacks the control under the text instead of beside it (§11's wrap).
fn row_extent(
    row: &SettingsRowModel,
    narrow: bool,
    desc_w: f32,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> (f32, Vec<String>) {
    match row {
        SettingsRowModel::Group { .. } => (28.0 * s, Vec::new()),
        SettingsRowModel::Notice { text } => {
            let lines = wrap_text(text, DESC_PX * s, desc_w, measure);
            ((lines.len() as f32 * LINE_H + 22.0) * s, lines)
        }
        SettingsRowModel::Unknown { .. } => (46.0 * s, Vec::new()),
        SettingsRowModel::Setting { description, value, .. } => {
            let lines = wrap_text(description, DESC_PX * s, desc_w, measure);
            let text_h = 18.0 + lines.len() as f32 * LINE_H + 18.0;
            let control_h = control_height(value);
            let h = if narrow {
                text_h + 8.0 + control_h
            } else {
                text_h.max(control_h)
            };
            ((h + 2.0 * ROW_VPAD) * s, lines)
        }
    }
}

pub fn settings_screen(
    model: &SettingsScreenModel,
    area: [f32; 4],
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;

    // Opaque ground over the whole grid area — a screen, not a scrim — and
    // one swallow region so nothing falls through to the grid beneath.
    out.rects.push(RectInstance::filled(area, colors.bg_opaque, area));
    out.hit.push(area, HitRegion::SettingsPanel);

    rail(model, area, colors, s, measure, out);

    // Content column, right of the rail.
    let rail_w = (RAIL_W * s).min(area[2] * 0.4);
    let content = [area[0] + rail_w, area[1], (area[2] - rail_w).max(0.0), area[3]];
    let cx = content[0] + CONTENT_X * s;
    let cw = (content[2] - 2.0 * CONTENT_X * s).max(0.0);

    // Header: group name, dotted prefix, lede.
    let mut y = content[1] + HEADER_PAD_TOP * s;
    let heading_px = HEADING_PX * s;
    y += heading_px;
    let hw = measure(&model.heading, heading_px, true, -0.01 * heading_px);
    out.texts.push(TextRun {
        text: model.heading.clone(),
        pos: [cx, y],
        max_width: cw * 0.7,
        color: colors.text_active,
        clip: content,
        px: heading_px,
        bold: true,
        tracking: -0.01 * heading_px,
    });
    if !model.prefix.is_empty() {
        out.texts.push(TextRun {
            text: model.prefix.clone(),
            pos: [cx + hw.min(cw * 0.7) + 12.0 * s, y],
            max_width: (cw - hw - 12.0 * s).max(0.0),
            color: colors.text_faint,
            clip: content,
            px: KEY_PX * s,
            bold: false,
            tracking: 0.0,
        });
    }
    y += 8.0 * s;
    for line in wrap_text(&model.lede, LEDE_PX * s, (520.0 * s).min(cw), measure) {
        y += LINE_H * s;
        out.texts.push(TextRun {
            text: line,
            pos: [cx, y],
            max_width: cw,
            color: colors.text_inactive,
            clip: content,
            px: LEDE_PX * s,
            bold: false,
            tracking: 0.0,
        });
    }
    y += HEADER_PAD_BOTTOM * s;
    out.rects.push(RectInstance::filled(
        [content[0], y, content[2], HAIRLINE * s],
        colors.hairline_soft,
        content,
    ));
    let rows_top = y + HAIRLINE * s;

    // Footer bar, fixed at the bottom; rows scroll between.
    let footer_y = area[1] + area[3] - FOOTER_H * s;
    footer(model, content, footer_y, colors, s, measure, out);

    let rows_clip = [content[0], rows_top, content[2], (footer_y - rows_top).max(0.0)];

    // §11's responsive wrap: with both the session sidebar and the rail
    // present the content column is under ~400 logical px, and the control
    // drops to its own line rather than crushing the label column.
    let narrow = cw / s < WRAP_AT;
    let desc_w = if narrow { cw } else { (cw - CONTROL_W * s - 20.0 * s).max(60.0 * s) }
        .min(420.0 * s);

    // Extents first: ensure-visible needs the selected row's offset before
    // anything draws.
    let mut tops = Vec::with_capacity(model.rows.len());
    let mut descs = Vec::with_capacity(model.rows.len());
    let mut content_h = 0.0f32;
    for row in &model.rows {
        let (h, lines) = row_extent(row, narrow, desc_w, s, measure);
        tops.push(content_h);
        descs.push(lines);
        content_h += h;
    }
    content_h += 12.0 * s;
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let mut scroll = model.scroll.clamp(0.0, max_scroll);
    if model.ensure_visible {
        if let (Some(top), Some(row)) = (tops.get(model.selected), model.rows.get(model.selected))
        {
            let bottom = top + row_extent(row, narrow, desc_w, s, measure).0;
            if *top < scroll {
                scroll = *top;
            } else if bottom > scroll + rows_clip[3] {
                scroll = bottom - rows_clip[3];
            }
            scroll = scroll.clamp(0.0, max_scroll);
        }
    }
    out.settings_scroll = scroll;

    if model.rows.is_empty() {
        if let Some(empty) = &model.empty {
            out.texts.push(TextRun {
                text: empty.clone(),
                pos: [cx, rows_top + 40.0 * s],
                max_width: cw,
                color: colors.text_faint,
                clip: rows_clip,
                px: LEDE_PX * s,
                bold: false,
                tracking: 0.0,
            });
        }
    }

    // The open dropdown's anchor, captured while its row draws.
    let mut menu_anchor: Option<[f32; 4]> = None;

    for (i, row) in model.rows.iter().enumerate() {
        let y = rows_top + tops[i] - scroll;
        let h = model
            .rows
            .get(i + 1)
            .map_or(content_h - 12.0 * s, |_| tops[i + 1])
            - tops[i];
        if y + h < rows_clip[1] || y > rows_clip[1] + rows_clip[3] {
            continue;
        }
        let band = [content[0], y, content[2], h];
        let Some(visible) = intersect(band, rows_clip) else { continue };

        match row {
            SettingsRowModel::Group { title } => {
                // Rare here (the rail is the grouping), but a schema group
                // outside GROUP_ORDER still labels itself.
                out.texts.push(TextRun {
                    text: title.clone(),
                    pos: [cx, baseline_in(y, h, LEDE_PX * s)],
                    max_width: cw,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: LEDE_PX * s,
                    bold: true,
                    tracking: 0.0,
                });
            }
            SettingsRowModel::Notice { .. } => {
                // The text draws from the pre-wrapped lines in `descs`.
                let band_rect = [cx, y + 6.0 * s, cw, h - 12.0 * s];
                out.rects.push(RectInstance {
                    radii: [9.0 * s; 4],
                    border: colors.pill_warn_text,
                    border_width: HAIRLINE * s,
                    ..RectInstance::filled(band_rect, colors.pill_warn_bg, rows_clip)
                });
                let mut ty = y + 6.0 * s;
                for line in &descs[i] {
                    ty += LINE_H * s;
                    out.texts.push(TextRun {
                        text: line.clone(),
                        pos: [cx + 14.0 * s, ty],
                        max_width: cw - 28.0 * s,
                        color: colors.pill_warn_text,
                        clip: rows_clip,
                        px: DESC_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }
            }
            SettingsRowModel::Unknown { key, source, suggestion } => {
                let band_rect = [cx, y + 3.0 * s, cw, h - 6.0 * s];
                out.rects.push(RectInstance {
                    radii: [9.0 * s; 4],
                    border: colors.line,
                    border_width: HAIRLINE * s,
                    ..RectInstance::filled(band_rect, colors.panel_bg, rows_clip)
                });
                let base = baseline_in(band_rect[1], band_rect[3], 12.0 * s);
                let mut right = cx + cw - 14.0 * s;
                if let Some(sugg) = suggestion {
                    let text = format!("did you mean {sugg}?");
                    let tw = measure(&text, 11.0 * s, false, 0.0);
                    right -= tw;
                    out.texts.push(TextRun {
                        text,
                        pos: [right, base],
                        max_width: tw + 2.0,
                        color: colors.accent,
                        clip: rows_clip,
                        px: 11.0 * s,
                        bold: false,
                        tracking: 0.0,
                    });
                    right -= 12.0 * s;
                }
                let sw = measure(source, 11.0 * s, false, 0.0);
                right -= sw;
                out.texts.push(TextRun {
                    text: source.clone(),
                    pos: [right, base],
                    max_width: sw + 2.0,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
                out.texts.push(TextRun {
                    text: key.clone(),
                    pos: [cx + 14.0 * s, base],
                    max_width: (right - cx - 26.0 * s).max(0.0),
                    color: colors.text_active,
                    clip: rows_clip,
                    px: 12.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            SettingsRowModel::Setting {
                label,
                key,
                value,
                provenance,
                restart,
                inert,
                modified,
                ..
            } => {
                if i == model.selected {
                    out.rects.push(RectInstance::rounded(
                        [content[0] + 8.0 * s, y + 4.0 * s, content[2] - 16.0 * s, h - 8.0 * s],
                        8.0 * s,
                        colors.accent_soft,
                        rows_clip,
                    ));
                }
                out.hit.push(visible, HitRegion::SettingsRow(i));
                if i + 1 < model.rows.len() {
                    out.rects.push(RectInstance::filled(
                        [cx, y + h - HAIRLINE * s, cw, HAIRLINE * s],
                        colors.hairline_soft,
                        rows_clip,
                    ));
                }

                let top = y + ROW_VPAD * s;
                // The modified dot IS the reset button (§11): 5px accent,
                // transparent (and unhittable) at the default.
                let dot = [cx, top + 6.0 * s, 5.0 * s, 5.0 * s];
                if *modified {
                    out.rects.push(RectInstance::rounded(
                        dot,
                        2.5 * s,
                        colors.accent,
                        rows_clip,
                    ));
                    // A 5px dot is not a click target; the hit band is 16px.
                    let grab = [dot[0] - 5.0 * s, dot[1] - 5.0 * s, 16.0 * s, 16.0 * s];
                    if let Some(hit) = intersect(grab, rows_clip) {
                        out.hit.push(hit, HitRegion::SettingsReset(i));
                    }
                }
                let text_x = cx + 14.0 * s;
                let mut ty = top + 13.0 * s;
                out.texts.push(TextRun {
                    text: label.clone(),
                    pos: [text_x, ty],
                    max_width: desc_w,
                    color: colors.text_active,
                    clip: rows_clip,
                    px: LABEL_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                ty += 5.0 * s;
                for line in &descs[i] {
                    ty += LINE_H * s;
                    out.texts.push(TextRun {
                        text: line.clone(),
                        pos: [text_x, ty],
                        max_width: desc_w,
                        color: colors.text_inactive,
                        clip: rows_clip,
                        px: DESC_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }
                ty += 16.0 * s;
                let kw = measure(key, KEY_PX * s, false, 0.0);
                out.texts.push(TextRun {
                    text: key.clone(),
                    pos: [text_x, ty],
                    max_width: desc_w,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: KEY_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                // Chips ride the key line: provenance, then restart, then
                // the not-wired honesty tag.
                let mut chip_x = text_x + kw + 10.0 * s;
                let mut chip = |text: String, fg: LinearRgba, bg: Option<LinearRgba>| {
                    let tw = measure(&text, CHIP_PX * s, false, 0.0);
                    let pad = 6.0 * s;
                    if let Some(bg) = bg {
                        out.rects.push(RectInstance::rounded(
                            [chip_x, ty - 10.0 * s, tw + 2.0 * pad, 15.0 * s],
                            5.0 * s,
                            bg,
                            rows_clip,
                        ));
                    }
                    out.texts.push(TextRun {
                        text,
                        pos: [chip_x + pad, ty + 1.0 * s],
                        max_width: tw + 2.0,
                        color: fg,
                        clip: rows_clip,
                        px: CHIP_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                    chip_x += tw + 2.0 * pad + 8.0 * s;
                };
                if let Some((text, warn)) = provenance {
                    if *warn {
                        chip(text.clone(), colors.pill_warn_text, Some(colors.pill_warn_bg));
                    } else {
                        chip(text.clone(), colors.accent, Some(colors.accent_soft));
                    }
                }
                if *restart {
                    chip("needs a restart".into(), colors.pill_warn_text, Some(colors.pill_warn_bg));
                }
                if *inert {
                    chip("not applied yet".into(), colors.text_faint, None);
                }

                // The control column: right-aligned beside the text, or on
                // its own line under it when narrow.
                let control_h = control_height(value) * s;
                let control_right = cx + cw;
                let control_top = if narrow {
                    y + h - ROW_VPAD * s - control_h
                } else {
                    top
                };
                let anchor = draw_control(
                    out, value, i, model, colors, s, rows_clip, control_right, control_top,
                    measure,
                );
                if let Some(a) = anchor {
                    menu_anchor = Some(a);
                }
            }
        }
    }

    if let (Some(menu), Some(anchor)) = (&model.menu, menu_anchor) {
        dropdown_menu(menu, anchor, area, colors, s, measure, out);
    }
}

/// Draw one value cell right-aligned against `right` at `top`; returns the
/// dropdown anchor when this row's select pill is the open menu's.
#[allow(clippy::too_many_arguments)]
fn draw_control(
    out: &mut ChromeLayout,
    value: &SettingsValueCell,
    row: usize,
    model: &SettingsScreenModel,
    colors: &ChromeColors,
    s: f32,
    clip: [f32; 4],
    right: f32,
    top: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> Option<[f32; 4]> {
    let mono = 12.0 * s;
    let mut menu_anchor = None;
    match value {
        SettingsValueCell::Toggle { on } => {
            let track = [right - TOGGLE_W * s, top, TOGGLE_W * s, TOGGLE_H * s];
            let fill = if *on { colors.accent } else { colors.line };
            out.rects.push(RectInstance::rounded(track, TOGGLE_H * s / 2.0, fill, clip));
            let knob_d = 16.0 * s;
            let knob_x = if *on {
                track[0] + track[2] - knob_d - 3.0 * s
            } else {
                track[0] + 3.0 * s
            };
            let knob = if *on { colors.bg_opaque } else { colors.text_inactive };
            out.rects.push(RectInstance::rounded(
                [knob_x, track[1] + 3.0 * s, knob_d, knob_d],
                knob_d / 2.0,
                knob,
                clip,
            ));
            if let Some(hit) = intersect(track, clip) {
                out.hit.push(hit, HitRegion::SettingsToggle(row));
            }
        }
        SettingsValueCell::Segmented { options, selected } => {
            let pad = 10.0 * s;
            let widths: Vec<f32> = options
                .iter()
                .map(|o| measure(o, DESC_PX * s, false, 0.0) + 2.0 * pad)
                .collect();
            let total = widths.iter().sum::<f32>() + 6.0 * s;
            let boxr = [right - total, top, total, SEG_H * s];
            out.rects.push(RectInstance {
                radii: [9.0 * s; 4],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(boxr, colors.panel_bg, clip)
            });
            let mut x = boxr[0] + 3.0 * s;
            for (j, (option, w)) in options.iter().zip(&widths).enumerate() {
                let seg = [x, boxr[1] + 3.0 * s, *w, boxr[3] - 6.0 * s];
                let chosen = *selected == Some(j);
                if chosen {
                    out.rects.push(RectInstance::rounded(seg, 7.0 * s, colors.accent_soft, clip));
                }
                if let Some(hit) = intersect(seg, clip) {
                    out.hit.push(hit, HitRegion::SettingsSegment(row, j));
                }
                let tw = measure(option, DESC_PX * s, false, 0.0);
                out.texts.push(TextRun {
                    text: option.clone(),
                    pos: [x + (w - tw) / 2.0, baseline_in(seg[1], seg[3], DESC_PX * s)],
                    max_width: *w,
                    color: if chosen { colors.accent } else { colors.text_inactive },
                    clip,
                    px: DESC_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                x += w;
            }
        }
        SettingsValueCell::Select { value } => {
            let pill = [right - SELECT_W * s, top, SELECT_W * s, SELECT_H * s];
            out.rects.push(RectInstance {
                radii: [8.0 * s; 4],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(pill, colors.panel_bg, clip)
            });
            if let Some(hit) = intersect(pill, clip) {
                out.hit.push(hit, HitRegion::SettingsSelect(row));
            }
            out.texts.push(TextRun {
                text: value.clone(),
                pos: [pill[0] + 12.0 * s, baseline_in(pill[1], pill[3], mono)],
                max_width: pill[2] - 36.0 * s,
                color: colors.text_active,
                clip,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
            let vw = measure("\u{25be}", 10.0 * s, false, 0.0);
            out.texts.push(TextRun {
                text: "\u{25be}".into(),
                pos: [pill[0] + pill[2] - 12.0 * s - vw, baseline_in(pill[1], pill[3], 10.0 * s)],
                max_width: vw + 2.0,
                color: colors.text_faint,
                clip,
                px: 10.0 * s,
                bold: false,
                tracking: 0.0,
            });
            if model.menu.as_ref().is_some_and(|menu| menu.row == row) {
                menu_anchor = Some(pill);
            }
        }
        SettingsValueCell::Slider { frac, text } => {
            let value_w = SLIDER_VALUE_W * s;
            let track = [
                right - value_w - 12.0 * s - TRACK_W * s,
                top + 5.0 * s,
                TRACK_W * s,
                TRACK_H * s,
            ];
            out.settings_tracks.push((row, track));
            let grab = [track[0] - 7.0 * s, top - 6.0 * s, track[2] + 14.0 * s, 26.0 * s];
            if let Some(hit) = intersect(grab, clip) {
                out.hit.push(hit, HitRegion::SettingsSlider(row));
            }
            out.rects.push(RectInstance::rounded(track, 2.0 * s, colors.line, clip));
            out.rects.push(RectInstance::rounded(
                [track[0], track[1], track[2] * frac.clamp(0.0, 1.0), track[3]],
                2.0 * s,
                colors.accent,
                clip,
            ));
            let knob = 14.0 * s;
            out.rects.push(RectInstance::rounded(
                [
                    track[0] + (track[2] - knob) * frac.clamp(0.0, 1.0),
                    track[1] + track[3] / 2.0 - knob / 2.0,
                    knob,
                    knob,
                ],
                knob / 2.0,
                colors.text_active,
                clip,
            ));
            let tw = measure(text, mono, false, 0.0);
            out.texts.push(TextRun {
                text: text.clone(),
                pos: [right - tw, baseline_in(top - 3.0 * s, 20.0 * s, mono)],
                max_width: value_w + 2.0,
                color: colors.text_active,
                clip,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
        }
        SettingsValueCell::Stepper { text } => {
            let value_w = measure(text, mono, false, 0.0).max(60.0 * s) + 14.0 * s;
            let w = 2.0 * STEP_BTN * s + value_w;
            let boxr = [right - w, top, w, STEP_H * s];
            out.rects.push(RectInstance {
                radii: [8.0 * s; 4],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(boxr, colors.panel_bg, clip)
            });
            for (up, x) in [(false, boxr[0]), (true, boxr[0] + w - STEP_BTN * s)] {
                let btn = [x, boxr[1], STEP_BTN * s, boxr[3]];
                if let Some(hit) = intersect(btn, clip) {
                    out.hit.push(hit, HitRegion::SettingsStep(row, up));
                }
                // ASCII plus on purpose: U+FF0B resolves to nothing in the shipped
                // stacks and renders as tofu (measured in the screenshot pass).
                let glyph = if up { "+" } else { "\u{2212}" };
                let gw = measure(glyph, 12.0 * s, false, 0.0);
                out.texts.push(TextRun {
                    text: glyph.into(),
                    pos: [x + (btn[2] - gw) / 2.0, baseline_in(btn[1], btn[3], 12.0 * s)],
                    max_width: btn[2],
                    color: colors.text_inactive,
                    clip,
                    px: 12.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            let tw = measure(text, mono, false, 0.0);
            out.texts.push(TextRun {
                text: text.clone(),
                pos: [
                    boxr[0] + STEP_BTN * s + (value_w - tw) / 2.0,
                    baseline_in(boxr[1], boxr[3], mono),
                ],
                max_width: value_w,
                color: colors.text_active,
                clip,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
        }
        SettingsValueCell::Text { text } | SettingsValueCell::ReadOnly { text } => {
            let faint = matches!(value, SettingsValueCell::ReadOnly { .. });
            let vw = measure(text, mono, false, 0.0).min(CONTROL_W * s);
            out.texts.push(TextRun {
                text: text.clone(),
                pos: [right - vw, baseline_in(top, 18.0 * s, mono)],
                max_width: CONTROL_W * s,
                color: if faint { colors.text_faint } else { colors.text_active },
                clip,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
        }
        SettingsValueCell::Editing { buffer, error } => {
            // The caret is a character, not a rect: it inherits the text
            // clip and colour for free, and this is not a text editor.
            let text = format!("{buffer}\u{258f}");
            let vw = measure(&text, mono, false, 0.0).min(CONTROL_W * s);
            let color = if *error { colors.pill_warn_text } else { colors.text_active };
            out.texts.push(TextRun {
                text,
                pos: [right - vw, baseline_in(top, 18.0 * s, mono)],
                max_width: CONTROL_W * s,
                color,
                clip,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
        }
        SettingsValueCell::FontList { faces } => {
            let w = CONTROL_W * s;
            let x = right - w;
            let mut ry = top;
            for (j, face) in faces.iter().enumerate() {
                let item = [x, ry, w, LIST_ROW_H * s];
                out.rects.push(RectInstance {
                    radii: [8.0 * s; 4],
                    border: colors.line,
                    border_width: HAIRLINE * s,
                    ..RectInstance::filled(item, colors.panel_bg, clip)
                });
                if let Some(hit) = intersect(item, clip) {
                    out.hit.push(hit, HitRegion::SettingsListItem(row, j));
                }
                let ink = if face.fallback { colors.text_faint } else { colors.text_active };
                out.texts.push(TextRun {
                    text: "\u{283f}".into(),
                    pos: [x + 9.0 * s, baseline_in(ry, item[3], 11.0 * s)],
                    max_width: 14.0 * s,
                    color: colors.text_faint,
                    clip,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
                let mut text_right = x + w - 26.0 * s;
                if face.fallback {
                    let tag_w = measure("fallback", 9.0 * s, false, 0.0);
                    text_right -= tag_w;
                    out.texts.push(TextRun {
                        text: "fallback".into(),
                        pos: [text_right, baseline_in(ry, item[3], 9.0 * s)],
                        max_width: tag_w + 2.0,
                        color: colors.text_faint,
                        clip,
                        px: 9.0 * s,
                        bold: false,
                        tracking: 0.0,
                    });
                    text_right -= 8.0 * s;
                }
                out.texts.push(TextRun {
                    text: face.family.clone(),
                    pos: [x + 26.0 * s, baseline_in(ry, item[3], 11.5 * s)],
                    max_width: (text_right - x - 26.0 * s).max(0.0),
                    color: ink,
                    clip,
                    px: 11.5 * s,
                    bold: false,
                    tracking: 0.0,
                });
                list_remove(out, colors, s, clip, row, j, [x + w - 24.0 * s, ry, 24.0 * s, item[3]], measure);
                ry += (LIST_ROW_H + LIST_GAP) * s;
            }
            list_add(out, colors, s, clip, row, [x, ry, w, LIST_ROW_H * s], "+ Add a family", measure);
        }
        SettingsValueCell::TagList { tags } => {
            // Chips flow right-to-left from the column's right edge so the
            // add chip is always visible; overflow clips at the column.
            let mut chip_right = right;
            let add_w = measure("+ tag", 11.0 * s, false, 0.0) + 20.0 * s;
            let add = [right - add_w, top, add_w, CHIP_H * s];
            dashed_border(&mut out.rects, add, s, colors.line, clip);
            if let Some(hit) = intersect(add, clip) {
                out.hit.push(hit, HitRegion::SettingsListAdd(row));
            }
            out.texts.push(TextRun {
                text: "+ tag".into(),
                pos: [add[0] + 10.0 * s, baseline_in(add[1], add[3], 11.0 * s)],
                max_width: add_w,
                color: colors.text_faint,
                clip,
                px: 11.0 * s,
                bold: false,
                tracking: 0.0,
            });
            chip_right -= add_w + 8.0 * s;
            let col_left = right - CONTROL_W * s;
            for (j, tag) in tags.iter().enumerate().rev() {
                let tw = measure(tag, 11.0 * s, false, 0.0);
                let xw = measure("\u{d7}", 11.0 * s, false, 0.0);
                let w = tw + xw + 26.0 * s;
                let chip = [chip_right - w, top, w, CHIP_H * s];
                if chip[0] < col_left {
                    break;
                }
                out.rects.push(RectInstance::rounded(chip, 7.0 * s, colors.accent_soft, clip));
                out.texts.push(TextRun {
                    text: tag.clone(),
                    pos: [chip[0] + 9.0 * s, baseline_in(chip[1], chip[3], 11.0 * s)],
                    max_width: tw + 2.0,
                    color: colors.accent,
                    clip,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
                let xr = [chip[0] + w - xw - 12.0 * s, chip[1], xw + 12.0 * s, chip[3]];
                if let Some(hit) = intersect(xr, clip) {
                    out.hit.push(hit, HitRegion::SettingsListRemove(row, j));
                }
                out.texts.push(TextRun {
                    text: "\u{d7}".into(),
                    pos: [xr[0] + 4.0 * s, baseline_in(chip[1], chip[3], 11.0 * s)],
                    max_width: xw + 2.0,
                    color: colors.text_faint,
                    clip,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
                chip_right -= w + 6.0 * s;
            }
        }
        SettingsValueCell::KeyValue { entries } => {
            let w = CONTROL_W * s;
            let x = right - w;
            let mut ry = top;
            for (j, (k, v)) in entries.iter().enumerate() {
                let key_w = w * 0.45;
                out.rects.push(RectInstance {
                    radii: [8.0 * s, 0.0, 0.0, 8.0 * s],
                    ..RectInstance::filled([x, ry, key_w, LIST_ROW_H * s], colors.accent_soft, clip)
                });
                out.rects.push(RectInstance {
                    radii: [0.0, 8.0 * s, 8.0 * s, 0.0],
                    border: colors.line,
                    border_width: HAIRLINE * s,
                    ..RectInstance::filled(
                        [x + key_w, ry, w - key_w, LIST_ROW_H * s],
                        colors.panel_bg,
                        clip,
                    )
                });
                out.texts.push(TextRun {
                    text: k.clone(),
                    pos: [x + 10.0 * s, baseline_in(ry, LIST_ROW_H * s, 11.0 * s)],
                    max_width: key_w - 14.0 * s,
                    color: colors.accent,
                    clip,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
                let (vtext, vcolor) = if v.is_empty() {
                    // Empty *unsets* the variable (wholesale replace, §11);
                    // say so instead of drawing a blank cell.
                    ("unset".to_string(), colors.text_faint)
                } else {
                    (v.clone(), colors.text_active)
                };
                out.texts.push(TextRun {
                    text: vtext,
                    pos: [x + key_w + 10.0 * s, baseline_in(ry, LIST_ROW_H * s, 11.0 * s)],
                    max_width: (w - key_w - 40.0 * s).max(0.0),
                    color: vcolor,
                    clip,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
                list_remove(out, colors, s, clip, row, j, [x + w - 24.0 * s, ry, 24.0 * s, LIST_ROW_H * s], measure);
                ry += (LIST_ROW_H + LIST_GAP) * s;
            }
            list_add(out, colors, s, clip, row, [x, ry, w, LIST_ROW_H * s], "+ Add an entry", measure);
        }
    }
    menu_anchor
}

/// A list item's × affordance.
#[allow(clippy::too_many_arguments)]
fn list_remove(
    out: &mut ChromeLayout,
    colors: &ChromeColors,
    s: f32,
    clip: [f32; 4],
    row: usize,
    item: usize,
    rect: [f32; 4],
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) {
    if let Some(hit) = intersect(rect, clip) {
        out.hit.push(hit, HitRegion::SettingsListRemove(row, item));
    }
    let xw = measure("\u{d7}", 12.0 * s, false, 0.0);
    out.texts.push(TextRun {
        text: "\u{d7}".into(),
        pos: [rect[0] + (rect[2] - xw) / 2.0, baseline_in(rect[1], rect[3], 12.0 * s)],
        max_width: rect[2],
        color: colors.text_faint,
        clip,
        px: 12.0 * s,
        bold: false,
        tracking: 0.0,
    });
}

/// A list widget's dashed add row.
#[allow(clippy::too_many_arguments)]
fn list_add(
    out: &mut ChromeLayout,
    colors: &ChromeColors,
    s: f32,
    clip: [f32; 4],
    row: usize,
    rect: [f32; 4],
    label: &str,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) {
    dashed_border(&mut out.rects, rect, s, colors.line, clip);
    if let Some(hit) = intersect(rect, clip) {
        out.hit.push(hit, HitRegion::SettingsListAdd(row));
    }
    let tw = measure(label, 11.0 * s, false, 0.0);
    out.texts.push(TextRun {
        text: label.to_string(),
        pos: [rect[0] + (rect[2] - tw) / 2.0, baseline_in(rect[1], rect[3], 11.0 * s)],
        max_width: rect[2],
        color: colors.text_faint,
        clip,
        px: 11.0 * s,
        bold: false,
        tracking: 0.0,
    });
}

/// The 288px dropdown (§11): ✓, label, the kebab wire value in mono, and
/// the variant's doc comment underneath. Drawn after every row so it sits
/// on top; its hit regions likewise outrank the rows beneath.
fn dropdown_menu(
    menu: &super::model::SettingsMenuModel,
    anchor: [f32; 4],
    area: [f32; 4],
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let row_h = |o: &super::model::SettingsMenuOption| {
        if o.doc.is_empty() { 30.0 * s } else { 46.0 * s }
    };
    let pad = 6.0 * s;
    let h = menu.options.iter().map(row_h).sum::<f32>() + 2.0 * pad;
    let w = MENU_W * s;
    let x = (anchor[0] + anchor[2] - w).max(area[0] + 8.0 * s);
    // Below the pill; above it when there is no room.
    let mut y = anchor[1] + anchor[3] + 4.0 * s;
    if y + h > area[1] + area[3] {
        y = (anchor[1] - h - 4.0 * s).max(area[1] + 8.0 * s);
    }
    let panel = [x, y, w, h];
    out.rects.push(RectInstance {
        radii: [MENU_RADIUS * s; 4],
        border: colors.line,
        border_width: HAIRLINE * s,
        shadow_blur: 18.0 * s,
        shadow_alpha: colors.shadow_alpha,
        ..RectInstance::filled(panel, colors.panel_bg, area)
    });

    let mut ry = y + pad;
    for (j, option) in menu.options.iter().enumerate() {
        let rh = row_h(option);
        let rect = [x + 4.0 * s, ry, w - 8.0 * s, rh];
        if j == menu.selected {
            out.rects.push(RectInstance::rounded(rect, 7.0 * s, colors.accent_soft, area));
        }
        out.hit.push(rect, HitRegion::SettingsMenuRow(j));
        let base = baseline_in(ry, 30.0 * s, DESC_PX * s);
        if menu.current == Some(j) {
            out.texts.push(TextRun {
                text: "\u{2713}".into(),
                pos: [x + 12.0 * s, base],
                max_width: 14.0 * s,
                color: colors.accent,
                clip: area,
                px: 11.0 * s,
                bold: false,
                tracking: 0.0,
            });
        }
        out.texts.push(TextRun {
            text: option.label.clone(),
            pos: [x + 30.0 * s, base],
            max_width: w * 0.5,
            color: colors.text_active,
            clip: area,
            px: DESC_PX * s,
            bold: false,
            tracking: 0.0,
        });
        let vw = measure(&option.value, KEY_PX * s, false, 0.0);
        out.texts.push(TextRun {
            text: option.value.clone(),
            pos: [x + w - 14.0 * s - vw, base],
            max_width: vw + 2.0,
            color: colors.text_faint,
            clip: area,
            px: KEY_PX * s,
            bold: false,
            tracking: 0.0,
        });
        if !option.doc.is_empty() {
            out.texts.push(TextRun {
                text: option.doc.clone(),
                pos: [x + 30.0 * s, ry + 40.0 * s],
                max_width: w - 44.0 * s,
                color: colors.text_faint,
                clip: area,
                px: 10.5 * s,
                bold: false,
                tracking: 0.0,
            });
        }
        ry += rh;
    }
}

/// The category rail (§11): filter pill, category rows with modified
/// counts, and the generated-from-the-schema footer note.
fn rail(
    model: &SettingsScreenModel,
    area: [f32; 4],
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let w = (RAIL_W * s).min(area[2] * 0.4);
    let rail = [area[0], area[1], w, area[3]];
    out.rects.push(RectInstance::filled(rail, colors.block_header_bg, area));
    out.rects.push(RectInstance::filled(
        [area[0] + w - HAIRLINE * s, area[1], HAIRLINE * s, area[3]],
        colors.hairline_soft,
        area,
    ));

    // Filter pill — the same 30px pill as the sidebar search, `/` in mono.
    let pill = [
        area[0] + RAIL_PAD * s,
        area[1] + 10.0 * s,
        w - 2.0 * RAIL_PAD * s,
        FILTER_H * s,
    ];
    out.rects.push(RectInstance {
        radii: [7.0 * s; 4],
        border: if model.filter.is_empty() { colors.line } else { colors.accent },
        border_width: HAIRLINE * s,
        ..RectInstance::filled(pill, colors.panel_bg, area)
    });
    out.hit.push(pill, HitRegion::SettingsFilter);
    let slash_w = measure("/", 11.0 * s, false, 0.0);
    out.texts.push(TextRun {
        text: "/".into(),
        pos: [pill[0] + 10.0 * s, baseline_in(pill[1], pill[3], 11.0 * s)],
        max_width: slash_w + 2.0,
        color: colors.text_faint,
        clip: area,
        px: 11.0 * s,
        bold: false,
        tracking: 0.0,
    });
    let (ftext, fcolor) = if model.filter.is_empty() {
        ("Filter settings".to_string(), colors.text_faint)
    } else {
        (model.filter.clone(), colors.text_active)
    };
    out.texts.push(TextRun {
        text: ftext,
        pos: [pill[0] + 10.0 * s + slash_w + 8.0 * s, baseline_in(pill[1], pill[3], 12.0 * s)],
        max_width: (pill[2] - slash_w - 30.0 * s).max(0.0),
        color: fcolor,
        clip: area,
        px: 12.0 * s,
        bold: false,
        tracking: 0.0,
    });

    // The footer note — the schema-generated promise, wrapped.
    let note = "Every field is generated from the config schema — a new setting appears \
                here with no UI change.";
    let note_lines = wrap_text(note, KEY_PX * s, w - 2.0 * RAIL_PAD * s, measure);
    let note_h = note_lines.len() as f32 * 15.0 * s + 20.0 * s;
    let note_top = area[1] + area[3] - note_h;
    out.rects.push(RectInstance::filled(
        [area[0], note_top, w, HAIRLINE * s],
        colors.hairline_soft,
        area,
    ));
    let mut ny = note_top + 6.0 * s;
    for line in note_lines {
        ny += 15.0 * s;
        out.texts.push(TextRun {
            text: line,
            pos: [area[0] + RAIL_PAD * s, ny],
            max_width: w - 2.0 * RAIL_PAD * s,
            color: colors.text_faint,
            clip: area,
            px: KEY_PX * s,
            bold: false,
            tracking: 0.0,
        });
    }

    // Category rows.
    let mut y = pill[1] + pill[3] + 10.0 * s;
    let rows_clip = [area[0], y, w, (note_top - y).max(0.0)];
    for (i, cat) in model.categories.iter().enumerate() {
        let row = [area[0] + RAIL_PAD * s, y, w - 2.0 * RAIL_PAD * s, CAT_ROW_H * s];
        let selected = i == model.selected_category;
        if selected {
            out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.accent_soft, rows_clip));
        }
        if let Some(hit) = intersect(row, rows_clip) {
            out.hit.push(hit, HitRegion::SettingsCategory(i));
        }
        out.texts.push(TextRun {
            text: cat.label.clone(),
            pos: [row[0] + 10.0 * s, baseline_in(y, row[3], 12.5 * s)],
            max_width: row[2] - 40.0 * s,
            color: if selected { colors.accent } else { colors.text_inactive },
            clip: rows_clip,
            px: 12.5 * s,
            bold: false,
            tracking: 0.0,
        });
        // The modified count, blank at zero (§11).
        if cat.modified > 0 {
            let text = cat.modified.to_string();
            let tw = measure(&text, CHIP_PX * s, false, 0.0);
            out.texts.push(TextRun {
                text,
                pos: [row[0] + row[2] - 10.0 * s - tw, baseline_in(y, row[3], CHIP_PX * s)],
                max_width: tw + 2.0,
                color: if selected { colors.accent } else { colors.text_faint },
                clip: rows_clip,
                px: CHIP_PX * s,
                bold: false,
                tracking: 0.0,
            });
        }
        y += (CAT_ROW_H + CAT_GAP) * s;
    }
}

/// The footer bar (§11): accent dot, the modified count as a sentence, the
/// config file's path, and `Edit as TOML`.
fn footer(
    model: &SettingsScreenModel,
    content: [f32; 4],
    footer_y: f32,
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let bar = [content[0], footer_y, content[2], FOOTER_H * s];
    out.rects.push(RectInstance::filled(bar, colors.block_header_bg, content));
    out.rects.push(RectInstance::filled(
        [bar[0], bar[1], bar[2], HAIRLINE * s],
        colors.hairline_soft,
        content,
    ));

    // Right side first — the sentence budgets against where the path starts,
    // or a long %AppData% path draws straight through it.
    let mut right = bar[0] + bar[2] - CONTENT_X * s;
    let edit = "Edit as TOML";
    let ew = measure(edit, DESC_PX * s, false, 0.0);
    right -= ew;
    let edit_rect = [right - 6.0 * s, bar[1], ew + 12.0 * s, bar[3]];
    out.hit.push(edit_rect, HitRegion::SettingsEditToml);
    out.texts.push(TextRun {
        text: edit.into(),
        pos: [right, baseline_in(bar[1], bar[3], DESC_PX * s)],
        max_width: ew + 2.0,
        color: colors.text_inactive,
        clip: content,
        px: DESC_PX * s,
        bold: false,
        tracking: 0.0,
    });
    right -= 14.0 * s;
    let pw = measure(&model.config_path, KEY_PX * s, false, 0.0).min(bar[2] * 0.35);
    right -= pw;
    out.texts.push(TextRun {
        text: model.config_path.clone(),
        pos: [right, baseline_in(bar[1], bar[3], KEY_PX * s)],
        max_width: pw + 2.0,
        color: colors.text_faint,
        clip: content,
        px: KEY_PX * s,
        bold: false,
        tracking: 0.0,
    });

    let x = bar[0] + CONTENT_X * s;
    out.rects.push(RectInstance::rounded(
        [x, bar[1] + (bar[3] - 5.0 * s) / 2.0, 5.0 * s, 5.0 * s],
        2.5 * s,
        colors.accent,
        content,
    ));
    let sentence = match model.modified_total {
        0 => "Every setting is at its default".to_string(),
        1 => "1 setting differs from the defaults — click a dot to reset".to_string(),
        n => format!("{n} settings differ from the defaults — click a dot to reset"),
    };
    out.texts.push(TextRun {
        text: sentence,
        pos: [x + 13.0 * s, baseline_in(bar[1], bar[3], 11.0 * s)],
        max_width: (right - x - 27.0 * s).max(0.0),
        color: colors.text_inactive,
        clip: content,
        px: 11.0 * s,
        bold: false,
        tracking: 0.0,
    });
}

fn intersect(r: [f32; 4], c: [f32; 4]) -> Option<[f32; 4]> {
    let x0 = r[0].max(c[0]);
    let y0 = r[1].max(c[1]);
    let x1 = (r[0] + r[2]).min(c[0] + c[2]);
    let y1 = (r[1] + r[3]).min(c[1] + c[3]);
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        SettingsCategoryModel, SettingsMenuModel, SettingsMenuOption,
    };
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0)
    }

    fn metrics(width: f32, height: f32) -> ChromeMetrics {
        ChromeMetrics {
            width,
            height,
            scale: 1.0,
            strip_height: 38.0,
            sidebar_width: 262.0,
            line_height: 18.0,
            baseline: 14.0,
            font_px: 13.0,
        }
    }

    fn measure(text: &str, px: f32, _b: bool, _t: f32) -> f32 {
        text.chars().count() as f32 * px * 0.6
    }

    fn cell_row(value: SettingsValueCell, modified: bool) -> SettingsRowModel {
        SettingsRowModel::Setting {
            label: "Font size".into(),
            key: "typography.size_pt".into(),
            description: "Point size of the grid font.".into(),
            value,
            provenance: None,
            restart: false,
            inert: false,
            modified,
        }
    }

    fn model(rows: Vec<SettingsRowModel>) -> SettingsScreenModel {
        SettingsScreenModel {
            categories: vec![
                SettingsCategoryModel { label: "Text".into(), modified: 2 },
                SettingsCategoryModel { label: "Unknown keys".into(), modified: 0 },
            ],
            selected_category: 0,
            heading: "Text".into(),
            prefix: "typography".into(),
            lede: "Font stack, size, and the cell geometry that follows from them.".into(),
            rows,
            empty: None,
            selected: 0,
            filter: String::new(),
            scroll: 0.0,
            ensure_visible: false,
            modified_total: 3,
            config_path: "~/.config/zesterm/config.toml".into(),
            menu: None,
        }
    }

    fn lay(model: &SettingsScreenModel, w: f32, h: f32) -> ChromeLayout {
        let mut out = ChromeLayout::default();
        settings_screen(model, [0.0, 46.0, w, h], &colors(), &metrics(w, h + 46.0), &mut measure, &mut out);
        out
    }

    #[test]
    fn the_rail_the_rows_and_the_footer_all_answer() {
        let m = model(vec![
            cell_row(SettingsValueCell::Toggle { on: true }, false),
            cell_row(SettingsValueCell::Stepper { text: "14 pt".into() }, true),
        ]);
        let l = lay(&m, 1000.0, 700.0);

        let mut seen_cat = false;
        let mut seen_row = false;
        let mut seen_reset = false;
        let mut seen_step = false;
        let mut seen_toggle = false;
        let mut seen_toml = false;
        for x in (0..1000).step_by(3) {
            for y in (46..746).step_by(3) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::SettingsCategory(_)) => seen_cat = true,
                    Some(HitRegion::SettingsRow(_)) => seen_row = true,
                    Some(HitRegion::SettingsReset(1)) => seen_reset = true,
                    Some(HitRegion::SettingsStep(1, _)) => seen_step = true,
                    Some(HitRegion::SettingsToggle(0)) => seen_toggle = true,
                    Some(HitRegion::SettingsEditToml) => seen_toml = true,
                    Some(HitRegion::SettingsReset(0)) => {
                        panic!("an unmodified row's dot must not take clicks")
                    }
                    Some(
                        HitRegion::SettingsPanel
                        | HitRegion::SettingsFilter
                        | HitRegion::SettingsSlider(_)
                        | HitRegion::SettingsToggle(_)
                        | HitRegion::SettingsStep(..),
                    ) => {}
                    None => {}
                    other => panic!("({x},{y}) escaped the settings screen: {other:?}"),
                }
            }
        }
        assert!(seen_cat, "category rows are clickable");
        assert!(seen_row, "setting rows are clickable");
        assert!(seen_reset, "the modified dot is the reset button");
        assert!(seen_step, "the stepper's − and ＋ are clickable");
        assert!(seen_toggle, "the toggle track flips");
        assert!(seen_toml, "'Edit as TOML' opens the file");
    }

    #[test]
    fn narrow_columns_drop_the_control_to_its_own_line() {
        // §11: with the session sidebar and the rail both present the
        // content column is under 400 logical px, and the control wraps
        // under the text instead of crushing the label column. The
        // observable: the row grows by the control's height.
        let rows = vec![cell_row(
            SettingsValueCell::Segmented {
                options: vec!["Top".into(), "Left".into()],
                selected: Some(0),
            },
            false,
        )];
        let wide = model(rows.clone());
        let narrow = model(rows);

        let wide_l = lay(&wide, 1000.0, 700.0);
        // 214 rail + 60 margins leaves ~300px of content column.
        let narrow_l = lay(&narrow, 560.0, 700.0);

        let row_rect = |l: &ChromeLayout| {
            let mut top = f32::MAX;
            let mut bottom = f32::MIN;
            for y in 46..746 {
                if matches!(l.hit.hit(600.0_f32.min(500.0), y as f32), Some(HitRegion::SettingsRow(0))) {
                    top = top.min(y as f32);
                    bottom = bottom.max(y as f32);
                }
            }
            bottom - top
        };
        let wide_h = row_rect(&wide_l);
        let narrow_h = row_rect(&narrow_l);
        assert!(
            narrow_h > wide_h + SEG_H / 2.0,
            "a narrow row must be taller than a wide one by about the control's height \
             (wide {wide_h}, narrow {narrow_h})"
        );

        // And the control still answers, on its own line.
        let mut seg_hit = false;
        for x in (0..560).step_by(2) {
            for y in (46..746).step_by(2) {
                if matches!(narrow_l.hit.hit(x as f32, y as f32), Some(HitRegion::SettingsSegment(0, _))) {
                    seg_hit = true;
                }
            }
        }
        assert!(seg_hit, "the wrapped control is still clickable");
    }

    #[test]
    fn the_open_menu_outranks_the_rows_beneath_it() {
        let mut m = model(vec![cell_row(
            SettingsValueCell::Select { value: "mica".into() },
            false,
        )]);
        m.menu = Some(SettingsMenuModel {
            row: 0,
            options: vec![
                SettingsMenuOption {
                    label: "None".into(),
                    value: "none".into(),
                    doc: String::new(),
                },
                SettingsMenuOption {
                    label: "Mica".into(),
                    value: "mica".into(),
                    doc: "The Windows 11 desktop wash.".into(),
                },
            ],
            current: Some(1),
            selected: 0,
        });
        let l = lay(&m, 1000.0, 700.0);
        let mut menu_rows = std::collections::HashSet::new();
        for x in (0..1000).step_by(2) {
            for y in (46..746).step_by(2) {
                if let Some(HitRegion::SettingsMenuRow(j)) = l.hit.hit(x as f32, y as f32) {
                    menu_rows.insert(j);
                }
            }
        }
        assert_eq!(menu_rows.len(), 2, "every option of the open menu answers as itself");
    }

    #[test]
    fn list_widgets_expose_remove_add_and_reorder_targets() {
        use super::super::model::SettingsFace;
        let m = model(vec![
            cell_row(
                SettingsValueCell::FontList {
                    faces: vec![
                        SettingsFace { family: "Cascadia Mono".into(), fallback: false },
                        SettingsFace { family: "Consolas".into(), fallback: true },
                    ],
                },
                false,
            ),
            cell_row(
                SettingsValueCell::KeyValue {
                    entries: vec![("FOO".into(), "bar".into()), ("GONE".into(), String::new())],
                },
                false,
            ),
            cell_row(SettingsValueCell::TagList { tags: vec!["-liga".into()] }, false),
        ]);
        let l = lay(&m, 1000.0, 700.0);
        let mut removes = std::collections::HashSet::new();
        let mut adds = std::collections::HashSet::new();
        let mut items = std::collections::HashSet::new();
        for x in (0..1000).step_by(2) {
            for y in (46..746).step_by(2) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::SettingsListRemove(r, i)) => {
                        removes.insert((r, i));
                    }
                    Some(HitRegion::SettingsListAdd(r)) => {
                        adds.insert(r);
                    }
                    Some(HitRegion::SettingsListItem(r, i)) => {
                        items.insert((r, i));
                    }
                    _ => {}
                }
            }
        }
        assert!(
            removes.contains(&(0, 0)) && removes.contains(&(0, 1)),
            "every font row has its ×: {removes:?}"
        );
        assert!(removes.contains(&(1, 0)) && removes.contains(&(1, 1)), "and every env entry");
        assert!(removes.contains(&(2, 0)), "and every tag chip");
        assert_eq!(adds.len(), 3, "each list widget has an add affordance: {adds:?}");
        assert!(
            items.contains(&(0, 0)) && items.contains(&(0, 1)),
            "font rows are drag targets — order is the setting"
        );
    }
}
