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
    /// Type size, physical pixels. The design's scale runs 9.5–21 logical;
    /// layout multiplies by `m.scale` before it lands here.
    pub px: f32,
    /// Semibold, for section labels and headings. Maps to the font stack's
    /// bold face (or synthesis) — the chrome has no in-between weights.
    pub bold: bool,
    /// Extra advance per cluster, physical px — the design's `.09em`
    /// uppercase section labels; 0.0 everywhere else.
    pub tracking: f32,
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
    /// The command palette's scroll, clamped — and possibly *adjusted*, when
    /// the model asked for the selection to be brought into view.
    pub palette_scroll: f32,
    /// The settings overlay's scroll, clamped — and possibly *adjusted*, when
    /// the model asked for the selection to be brought into view.
    pub settings_scroll: f32,
    /// Slider tracks by row index, exactly as drawn — a click's fraction is
    /// computed against these, so pointer and pixels cannot disagree.
    pub settings_tracks: Vec<(usize, [f32; 4])>,
}

// Logical-pixel constants, scaled at use. Named because the tests reason
// about them; not settings, because nobody should have to care. The values
// are the design handoff's (docs/design/client-ui/README.md) — change them
// there first or not at all.
const TAB_H: f32 = 34.0;
const TAB_MIN: f32 = 196.0;
const TAB_MAX: f32 = 240.0;
const TAB_GAP: f32 = 3.0;
const TAB_PAD: f32 = 11.0;
const TAB_INNER_GAP: f32 = 9.0;
const TAB_RADIUS: f32 = 9.0;
const ACCENT_RULE: f32 = 2.0;
const DOT: f32 = 6.0;
const TEXT_PAD: f32 = 8.0;
const RADIUS: f32 = 6.0;
const CLOSE: f32 = 16.0;
const NEW_TAB_W: f32 = 28.0;
const NEW_TAB_H: f32 = 30.0;
const PILL_H: f32 = 26.0;
const PILL_RADIUS: f32 = 7.0;
const PILL_PAD: f32 = 9.0;
const PILL_GAP: f32 = 6.0;
const PILL_HPAD: f32 = 5.0;
const HAIRLINE: f32 = 1.0;
const EDGE_PAD: f32 = 8.0;
const BAR_PAD: f32 = 12.0;
const TRAFFIC_PAD: f32 = 14.0;
const ROW_HPAD: f32 = 8.0;
// Sidebar (design screen 2).
const SIDEBAR_HEADER: f32 = 44.0;
const SEARCH_PAD: f32 = 10.0;
const SEARCH_H: f32 = 30.0;
const GROUP_HEADER_H: f32 = 26.0;
const GROUP_GAP: f32 = 14.0;
const SIDE_ROW_H: f32 = 44.0;
const FOOTER_H: f32 = 42.0;
const SLIM_BAR_H: f32 = 44.0;
const SLIM_PAD: f32 = 14.0;

/// The status bar's height, logical pixels. Public because `insets_at` must
/// subtract it from the grid.
pub const STATUS_H: f32 = 28.0;
const STATUS_HPAD: f32 = 14.0;

// The design's type scale, logical px.
const UI_BODY: f32 = 12.5;
const UI_SMALL: f32 = 11.0;
const UI_TAB_SUB: f32 = 9.5;
const UI_CHORD: f32 = 10.0;
const UI_STATUS: f32 = 10.5;

/// Baseline that vertically centres a run of `px`-sized text in a band.
/// 0.72·px approximates the ascent above baseline for the faces we ship;
/// exact per-face metrics would need the font here, and being one pixel
/// off is invisible while being *inconsistent* is not — every band uses
/// this one rule.
fn baseline_in(band_y: f32, band_h: f32, px: f32) -> f32 {
    band_y + (band_h + px * 0.72) / 2.0
}

/// The host-accent cycle: slot 0 (the local machine) is `success`, then
/// `info`, `magenta`, `warn` — the design's studio/crate/forge assignment
/// generalized. Wraps rather than running out.
fn host_accent(colors: &ChromeColors, slot: usize) -> LinearRgba {
    [colors.success, colors.info, colors.magenta, colors.warn][slot % 4]
}

/// A little status dot, as the SDF pipeline draws circles: a square rect
/// with radius d/2.
fn dot(rects: &mut Vec<RectInstance>, cx: f32, cy: f32, d: f32, color: LinearRgba, clip: [f32; 4]) {
    rects.push(RectInstance::rounded([cx - d / 2.0, cy - d / 2.0, d, d], d / 2.0, color, clip));
}

pub fn layout(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> ChromeLayout {
    let mut out = match model.position {
        TabsPosition::Top => horizontal(model, colors, m, measure),
        TabsPosition::Left => vertical(model, colors, m, measure),
    };
    if let Some(screen) = &model.screen {
        // Over the grid, under the modals: a screen is window content, not
        // an overlay, so the picker can still open above it.
        super::screens::screen_overlay(screen, model.grid_area, colors, m.scale, measure, &mut out);
    }
    if let Some(picker) = &model.picker {
        // Appended last on purpose: last drawn is topmost, and last pushed
        // wins the hit lookup — the same fact, stated once.
        picker_overlay(picker, colors, m, measure, &mut out);
    }
    if let Some(palette) = &model.palette {
        palette_overlay(palette, colors, m, measure, &mut out);
    }
    if let Some(settings) = &model.settings {
        settings_overlay(settings, colors, m, measure, &mut out);
    }
    out
}

// ⌘K palette geometry (design screen 6), logical px.
const PICKER_W: f32 = 620.0;
const PICKER_TOP: f32 = 88.0;
const PICKER_RADIUS: f32 = 14.0;
const PICKER_PAD: f32 = 8.0;
const PICKER_ROW_H: f32 = 34.0;
const PICKER_QUERY_H: f32 = 48.0;
const PICKER_FOOTER_H: f32 = 34.0;
const PICKER_MAX_H: f32 = 480.0;
const PICKER_MARGIN: f32 = 40.0;

// ⌘P command-palette geometry, logical px.
const PALETTE_W: f32 = 640.0;
const PALETTE_H: f32 = 500.0;
const PALETTE_ROW_H: f32 = 28.0;
const PALETTE_HEADER_H: f32 = 36.0;
const CHIP_HPAD: f32 = 8.0;
const CHIP_VPAD: f32 = 3.0;

fn picker_overlay(
    picker: &super::model::PickerModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    use super::model::PickerRow;
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    // The scrim: modal by construction — it catches everything the panel
    // does not.
    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::PickerScrim);

    let w = (PICKER_W * s).min(m.width * 0.92);
    let x = (m.width - w) / 2.0;
    let y = (PICKER_TOP * s).min(m.height * 0.15);

    // Heights: query row + rows (clamped) + footer.
    let row_h = |row: &PickerRow| match row {
        PickerRow::Group { .. } => 24.0 * s,
        _ => PICKER_ROW_H * s,
    };
    let content_h: f32 = picker.rows.iter().map(&row_h).sum::<f32>() + 2.0 * PICKER_PAD * s;
    let list_h = content_h.min(PICKER_MAX_H * s).min(m.height - y - 80.0 * s);
    let panel_h = PICKER_QUERY_H * s + list_h + PICKER_FOOTER_H * s;
    let panel = [x, y, w, panel_h];
    out.rects.push(RectInstance {
        radii: [PICKER_RADIUS * s; 4],
        border: colors.line,
        border_width: HAIRLINE * s,
        shadow_blur: 30.0 * s,
        shadow_alpha: colors.shadow_alpha,
        ..RectInstance::filled(panel, colors.panel_bg, no_clip)
    });
    // The panel between rows swallows clicks (pushed before the rows, which
    // out-rank it where they overlap).
    out.hit.push(panel, HitRegion::PickerPanel);

    // Query row: ❯, the query (or placeholder), a caret, and how many hosts
    // the search ran over.
    {
        let qy = y;
        let qh = PICKER_QUERY_H * s;
        let mut qx = x + 16.0 * s;
        let prompt_px = 14.0 * s;
        out.texts.push(TextRun {
            text: "\u{276f}".into(),
            pos: [qx, baseline_in(qy, qh, prompt_px)],
            max_width: 14.0 * s,
            color: colors.accent,
            clip: no_clip,
            px: prompt_px,
            bold: false,
            tracking: 0.0,
        });
        qx += 16.0 * s;
        let (qtext, qcolor) = if picker.filter.is_empty() {
            ("Search sessions, blocks, hosts".to_string(), colors.text_faint)
        } else {
            (picker.filter.clone(), colors.text_active)
        };
        let qw = measure(&qtext, prompt_px, false, 0.0).min(w * 0.6);
        out.texts.push(TextRun {
            text: qtext,
            pos: [qx, baseline_in(qy, qh, prompt_px)],
            max_width: qw,
            color: qcolor,
            clip: no_clip,
            px: prompt_px,
            bold: false,
            tracking: 0.0,
        });
        if !picker.filter.is_empty() {
            qx += qw;
        }
        // The caret: an 8×16 accent block. Blinking arrives with the
        // animation clock; a standing caret is honest until then.
        out.rects.push(RectInstance::filled(
            [qx + 2.0 * s, qy + (qh - 16.0 * s) / 2.0, 8.0 * s, 16.0 * s],
            colors.accent,
            no_clip,
        ));
        let hosts = match picker.hosts_searched {
            1 => "1 host searched".to_string(),
            n => format!("{n} hosts searched"),
        };
        let hw = measure(&hosts, UI_STATUS * s, false, 0.0);
        out.texts.push(TextRun {
            text: hosts,
            pos: [x + w - 16.0 * s - hw, baseline_in(qy, qh, UI_STATUS * s)],
            max_width: hw + 2.0,
            color: colors.text_faint,
            clip: no_clip,
            px: UI_STATUS * s,
            bold: false,
            tracking: 0.0,
        });
        out.rects.push(RectInstance::filled(
            [x, qy + qh - HAIRLINE * s, w, HAIRLINE * s],
            colors.hairline_soft,
            no_clip,
        ));
    }

    // The rows, clipped and scrolled.
    let list_top = y + PICKER_QUERY_H * s;
    let rows_clip = [x, list_top, w, list_h];
    let max_scroll = (content_h - list_h).max(0.0);
    let mut scroll = picker.scroll.clamp(0.0, max_scroll);

    // Keyboard navigation must never act on an off-screen row.
    if picker.ensure_visible {
        let mut top = PICKER_PAD * s;
        for (i, row) in picker.rows.iter().enumerate() {
            let h = row_h(row);
            if i == picker.selected {
                let above = top - scroll;
                let below = top + h - scroll - list_h;
                if above < 0.0 {
                    scroll += above;
                } else if below > 0.0 {
                    scroll += below;
                }
                break;
            }
            top += h;
        }
        scroll = scroll.clamp(0.0, max_scroll);
    }
    out.picker_scroll = scroll;

    let mut ry = list_top + PICKER_PAD * s - scroll;
    for (i, row) in picker.rows.iter().enumerate() {
        let h = row_h(row);
        let rect = [x + PICKER_PAD * s, ry, w - 2.0 * PICKER_PAD * s, h];
        ry += h;
        if ry < list_top || rect[1] > list_top + list_h {
            continue;
        }

        let selected = i == picker.selected;
        match row {
            PickerRow::Group { title } => {
                let px = UI_CHORD * s;
                let tracking = 0.09 * px;
                let title = title.to_uppercase();
                let tw = measure(&title, px, true, tracking);
                out.texts.push(TextRun {
                    text: title,
                    pos: [rect[0] + 10.0 * s, baseline_in(rect[1], h, px)],
                    max_width: tw + 2.0,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px,
                    bold: true,
                    tracking,
                });
                continue;
            }
            PickerRow::Nothing => {
                out.texts.push(TextRun {
                    text: "nothing matches".into(),
                    pos: [rect[0] + 10.0 * s, baseline_in(rect[1], h, UI_BODY * s)],
                    max_width: rect[2],
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
                continue;
            }
            _ => {}
        }

        if selected {
            out.rects.push(RectInstance::rounded(rect, 9.0 * s, colors.accent_soft, rows_clip));
        }
        if let Some(hit) = intersect(rect, rows_clip) {
            out.hit.push(hit, HitRegion::PickerRow(i));
        }

        let mut tx = rect[0] + 10.0 * s;
        let mut right = rect[0] + rect[2] - 10.0 * s;
        match row {
            PickerRow::Block { command, provenance, ok } => {
                let glyph_color = if *ok { colors.success } else { colors.danger };
                out.texts.push(TextRun {
                    text: "\u{21ba}".into(),
                    pos: [tx, baseline_in(rect[1], h, UI_SMALL * s)],
                    max_width: 14.0 * s,
                    color: glyph_color,
                    clip: rows_clip,
                    px: UI_SMALL * s,
                    bold: false,
                    tracking: 0.0,
                });
                tx += 18.0 * s;
                if selected {
                    let hint = "\u{23ce} re-run";
                    let hw = measure(hint, UI_CHORD * s, false, 0.0);
                    out.texts.push(TextRun {
                        text: hint.into(),
                        pos: [right - hw, baseline_in(rect[1], h, UI_CHORD * s)],
                        max_width: hw + 2.0,
                        color: colors.accent,
                        clip: rows_clip,
                        px: UI_CHORD * s,
                        bold: false,
                        tracking: 0.0,
                    });
                    right -= hw + 10.0 * s;
                }
                let pw = measure(provenance, UI_STATUS * s, false, 0.0);
                out.texts.push(TextRun {
                    text: provenance.clone(),
                    pos: [right - pw, baseline_in(rect[1], h, UI_STATUS * s)],
                    max_width: pw + 2.0,
                    color: if selected { colors.text_inactive } else { colors.text_faint },
                    clip: rows_clip,
                    px: UI_STATUS * s,
                    bold: false,
                    tracking: 0.0,
                });
                right -= pw + 10.0 * s;
                out.texts.push(TextRun {
                    text: command.clone(),
                    pos: [tx, baseline_in(rect[1], h, UI_BODY * s)],
                    max_width: (right - tx).max(0.0),
                    color: if selected { colors.text_active } else { colors.text_inactive },
                    clip: rows_clip,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            PickerRow::Session { title, detail, host, attached, attached_here } => {
                dot(
                    &mut out.rects,
                    tx + 2.5 * s,
                    rect[1] + h / 2.0,
                    5.0 * s,
                    if *attached { colors.success } else { colors.text_faint },
                    rows_clip,
                );
                tx += 15.0 * s;
                let hw = measure(host, UI_STATUS * s, false, 0.0);
                out.texts.push(TextRun {
                    text: host.clone(),
                    pos: [right - hw, baseline_in(rect[1], h, UI_STATUS * s)],
                    max_width: hw + 2.0,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: UI_STATUS * s,
                    bold: false,
                    tracking: 0.0,
                });
                right -= hw + 10.0 * s;
                let label = if *attached_here {
                    format!("{title} \u{b7} this window")
                } else {
                    title.clone()
                };
                let label_w = measure(&label, UI_BODY * s, false, 0.0).min((right - tx) * 0.6);
                out.texts.push(TextRun {
                    text: label,
                    pos: [tx, baseline_in(rect[1], h, UI_BODY * s)],
                    max_width: label_w,
                    color: if selected { colors.text_active } else { colors.text_inactive },
                    clip: rows_clip,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
                if !detail.is_empty() {
                    out.texts.push(TextRun {
                        text: detail.clone(),
                        pos: [
                            tx + label_w + 8.0 * s,
                            baseline_in(rect[1], h, 11.5 * s),
                        ],
                        max_width: (right - tx - label_w - 8.0 * s).max(0.0),
                        color: colors.text_faint,
                        clip: rows_clip,
                        px: 11.5 * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }
            }
            PickerRow::Host { label, presence, detail } => {
                let dot_color = match presence {
                    TabPresence::Online => colors.success,
                    TabPresence::Unreachable => colors.warn,
                    _ => colors.text_faint,
                };
                dot(&mut out.rects, tx + 2.5 * s, rect[1] + h / 2.0, 5.0 * s, dot_color, rows_clip);
                tx += 15.0 * s;
                let presence_word = match presence {
                    TabPresence::Online => "online",
                    TabPresence::Away => "away",
                    TabPresence::Unseen => "unseen",
                    TabPresence::Unreachable => "unreachable",
                };
                let prov = if detail.is_empty() {
                    presence_word.to_string()
                } else {
                    format!("{presence_word} \u{b7} {detail}")
                };
                let pw = measure(&prov, UI_STATUS * s, false, 0.0);
                out.texts.push(TextRun {
                    text: prov,
                    pos: [right - pw, baseline_in(rect[1], h, UI_STATUS * s)],
                    max_width: pw + 2.0,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: UI_STATUS * s,
                    bold: false,
                    tracking: 0.0,
                });
                right -= pw + 10.0 * s;
                out.texts.push(TextRun {
                    text: format!("New session on {label}"),
                    pos: [tx, baseline_in(rect[1], h, UI_BODY * s)],
                    max_width: (right - tx).max(0.0),
                    color: if selected { colors.text_active } else { colors.text_inactive },
                    clip: rows_clip,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            PickerRow::Action { name, chord } => {
                let cw = measure(chord, UI_CHORD * s, false, 0.0);
                out.texts.push(TextRun {
                    text: chord.clone(),
                    pos: [right - cw, baseline_in(rect[1], h, UI_CHORD * s)],
                    max_width: cw + 2.0,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: UI_CHORD * s,
                    bold: false,
                    tracking: 0.0,
                });
                right -= cw + 10.0 * s;
                out.texts.push(TextRun {
                    text: name.clone(),
                    pos: [tx, baseline_in(rect[1], h, UI_BODY * s)],
                    max_width: (right - tx).max(0.0),
                    color: if selected { colors.text_active } else { colors.text_inactive },
                    clip: rows_clip,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            PickerRow::Group { .. } | PickerRow::Nothing => unreachable!("handled above"),
        }
    }

    // Footer: the keys, spelled out.
    {
        let fy = y + panel_h - PICKER_FOOTER_H * s;
        let fh = PICKER_FOOTER_H * s;
        out.rects.push(RectInstance {
            radii: [0.0, 0.0, PICKER_RADIUS * s, PICKER_RADIUS * s],
            ..RectInstance::filled([x, fy, w, fh], colors.block_header_bg, no_clip)
        });
        out.rects.push(RectInstance::filled(
            [x, fy, w, HAIRLINE * s],
            colors.hairline_soft,
            no_clip,
        ));
        let px = UI_STATUS * s;
        let base = baseline_in(fy, fh, px);
        let mut fx = x + 16.0 * s;
        for (cap, label) in [
            ("\u{2191}\u{2193}", " navigate"),
            ("\u{23ce}", " run here"),
            ("\u{21e7}\u{23ce}", " run in its session"),
        ] {
            let cw = measure(cap, px, false, 0.0);
            out.texts.push(TextRun {
                text: cap.into(),
                pos: [fx, base],
                max_width: cw + 2.0,
                color: colors.text_inactive,
                clip: no_clip,
                px,
                bold: false,
                tracking: 0.0,
            });
            fx += cw;
            let lw = measure(label, px, false, 0.0);
            out.texts.push(TextRun {
                text: label.into(),
                pos: [fx, base],
                max_width: lw + 2.0,
                color: colors.text_faint,
                clip: no_clip,
                px,
                bold: false,
                tracking: 0.0,
            });
            fx += lw + 16.0 * s;
        }
        let esc = "esc";
        let dismiss = " dismiss";
        let ew = measure(esc, px, false, 0.0);
        let dw = measure(dismiss, px, false, 0.0);
        out.texts.push(TextRun {
            text: esc.into(),
            pos: [x + w - 16.0 * s - dw - ew, base],
            max_width: ew + 2.0,
            color: colors.text_inactive,
            clip: no_clip,
            px,
            bold: false,
            tracking: 0.0,
        });
        out.texts.push(TextRun {
            text: dismiss.into(),
            pos: [x + w - 16.0 * s - dw, base],
            max_width: dw + 2.0,
            color: colors.text_faint,
            clip: no_clip,
            px,
            bold: false,
            tracking: 0.0,
        });
    }
}

fn palette_overlay(
    palette: &super::model::PaletteModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    use super::model::PaletteRow;

    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    // Same modality recipe as the picker: the scrim swallows what the panel
    // does not, so the grid hears nothing while the palette is up.
    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::PaletteScrim);

    let w = (PALETTE_W * s).min(m.width - PICKER_MARGIN * s);
    let h = (PALETTE_H * s).min(m.height - PICKER_MARGIN * s);
    let panel = [(m.width - w) / 2.0, (m.height - h) / 2.5, w, h];
    let mut panel_rect = RectInstance::rounded(panel, PICKER_RADIUS * s, colors.panel_bg, no_clip);
    panel_rect.shadow_blur = 24.0 * s;
    panel_rect.shadow_alpha = colors.shadow_alpha;
    out.rects.push(panel_rect);
    // A click that misses every runnable row must not fall through to the
    // scrim and dismiss what the user is reading.
    out.hit.push(panel, HitRegion::PalettePanel);

    let filter_h = m.line_height + 2.0 * PICKER_PAD * s;
    let (filter_text, filter_color) = if palette.filter.is_empty() {
        ("type to run a command".to_string(), colors.text_faint)
    } else {
        (palette.filter.clone(), colors.text_active)
    };
    out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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

    if palette.rows.is_empty() && !palette.filter.is_empty() {
        // A filter that matches nothing must say so - a silently blank
        // panel reads as broken, not as empty.
        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
            text: format!("nothing matches \u{201c}{}\u{201d}", palette.filter),
            pos: [panel[0] + PICKER_PAD * s, text_baseline(m, rows_clip[1], PALETTE_ROW_H * s)],
            max_width: w - 2.0 * PICKER_PAD * s,
            color: colors.text_faint,
            clip: rows_clip,
        });
    }

    let row_h = |row: &PaletteRow| match row {
        PaletteRow::Group { .. } => PALETTE_HEADER_H * s,
        PaletteRow::Command { .. } => PALETTE_ROW_H * s,
    };
    // Row offsets before any drawing: ensure-visible needs the selected
    // row's extent to decide the scroll it draws with.
    let mut tops = Vec::with_capacity(palette.rows.len());
    let mut content_h = 0.0f32;
    for row in &palette.rows {
        tops.push(content_h);
        content_h += row_h(row);
    }
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let mut scroll = palette.scroll.clamp(0.0, max_scroll);
    if palette.ensure_visible {
        if let (Some(top), Some(row)) =
            (tops.get(palette.selected), palette.rows.get(palette.selected))
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
    out.palette_scroll = scroll;

    let left = panel[0] + PICKER_PAD * s;
    let right = panel[0] + w - PICKER_PAD * s;
    for (i, row) in palette.rows.iter().enumerate() {
        let y = rows_clip[1] + tops[i] - scroll;
        let band = [panel[0], y, w, row_h(row)];
        let Some(visible) = intersect(band, rows_clip) else { continue };

        match row {
            PaletteRow::Group { title } => {
                out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                    text: title.clone(),
                    // Bottom-aligned in its band so the title sits close to
                    // its rows rather than the previous group's.
                    pos: [
                        left,
                        text_baseline(m, y + (PALETTE_HEADER_H - PALETTE_ROW_H) * s, PALETTE_ROW_H * s),
                    ],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color: colors.text_faint,
                    clip: rows_clip,
                });
            }
            PaletteRow::Command { name, chord, runnable } => {
                if *runnable {
                    if i == palette.selected {
                        let chip =
                            [panel[0] + 4.0 * s, y + 1.0 * s, w - 8.0 * s, band[3] - 2.0 * s];
                        out.rects.push(RectInstance::rounded(
                            chip,
                            RADIUS * s,
                            colors.accent_soft,
                            rows_clip,
                        ));
                    }
                    out.hit.push(visible, HitRegion::PaletteRow(i));
                }
                let baseline = text_baseline(m, y, PALETTE_ROW_H * s);
                out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                    text: name.clone(),
                    pos: [left, baseline],
                    max_width: w * 0.6,
                    // Reference rows read as annotations, not as commands
                    // that mysteriously refuse to run.
                    color: if *runnable { colors.text_inactive } else { colors.text_faint },
                    clip: rows_clip,
                });
                if !chord.is_empty() {
                    // The chord, right-aligned in a keycap-look chip.
                    let chord_w = measure(chord, m.font_px, false, 0.0).min(w * 0.35);
                    let chip = [
                        right - chord_w - 2.0 * CHIP_HPAD * s,
                        y + CHIP_VPAD * s,
                        chord_w + 2.0 * CHIP_HPAD * s,
                        PALETTE_ROW_H * s - 2.0 * CHIP_VPAD * s,
                    ];
                    out.rects.push(RectInstance::rounded(
                        chip,
                        RADIUS * s,
                        colors.accent_soft,
                        rows_clip,
                    ));
                    out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                        text: chord.clone(),
                        pos: [right - chord_w - CHIP_HPAD * s, baseline],
                        max_width: w * 0.35,
                        color: colors.text_active,
                        clip: rows_clip,
                    });
                }
            }
        }
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
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
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
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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

    // A filter that matches nothing must say so — a silently blank panel
    // reads as broken, not as empty.
    if settings.rows.is_empty() && !settings.filter.is_empty() {
        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
            text: format!("nothing matches \u{201c}{}\u{201d}", settings.filter),
            pos: [panel[0] + PICKER_PAD * s, text_baseline(m, rows_clip[1], SETTINGS_ROW_H * s)],
            max_width: w - 2.0 * PICKER_PAD * s,
            color: colors.text_faint,
            clip: rows_clip,
        });
    }

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
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                    text: text.clone(),
                    pos: [left, text_baseline(m, y, band[3])],
                    max_width: w - 2.0 * PICKER_PAD * s,
                    color: colors.pill_warn_text,
                    clip: rows_clip,
                });
            }
            SettingsRowModel::Group { title } => {
                out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                    text: label.clone(),
                    pos: [label_x, baseline1],
                    max_width: w * 0.4,
                    color: colors.text_active,
                    clip: rows_clip,
                });

                // Second line: tags right, then the description in whatever
                // room is left — tags are the truth-telling part and must
                // never be overwritten by a long doc comment.
                let mut tag_x = right;
                let mut push_tag = |text: String, color, tag_x: &mut f32| {
                    let tw = measure(&text, m.font_px, false, 0.0).min(w * 0.35);
                    *tag_x -= tw;
                    out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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

                // The dotted key rides with the description so the user can
                // grep their config for exactly what this row is.
                let desc = if description.is_empty() {
                    key.clone()
                } else {
                    format!("{key} — {description}")
                };
                out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                    text: desc,
                    pos: [label_x, baseline2],
                    max_width: (tag_x - label_x - 12.0 * s).max(0.0),
                    color: colors.text_faint,
                    clip: rows_clip,
                });

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
                        let vw = measure(value, m.font_px, false, 0.0).min(w * 0.3);
                        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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
                        out.settings_tracks.push((i, track));
                        // The hit band is the first line's height, not the
                        // 4px track: nobody can click a hairline.
                        let grab = [track[0] - 6.0 * s, y, track[2] + 12.0 * s, line1_h];
                        if let Some(hit) = intersect(grab, rows_clip) {
                            out.hit.push(hit, HitRegion::SettingsSlider(i));
                        }
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
                        let tw = measure(text, m.font_px, false, 0.0).min(w * 0.15);
                        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                            text: text.clone(),
                            pos: [track[0] - tw - 8.0 * s, baseline1],
                            max_width: w * 0.15,
                            color: colors.text_active,
                            clip: rows_clip,
                        });
                    }
                    SettingsValueCell::Text { text } => {
                        let vw = measure(text, m.font_px, false, 0.0).min(w * 0.35);
                        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
                            text: text.clone(),
                            pos: [right - vw, baseline1],
                            max_width: w * 0.35,
                            color: colors.text_active,
                            clip: rows_clip,
                        });
                    }
                    SettingsValueCell::ReadOnly { text } => {
                        let vw = measure(text, m.font_px, false, 0.0).min(w * 0.35);
                        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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
                        let vw = measure(&text, m.font_px, false, 0.0).min(w * 0.35);
                        let color =
                            if *error { colors.pill_warn_text } else { colors.text_active };
                        out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
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
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
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
    // window drag like the rest of the empty strip. 14px of air after the
    // cluster, per the design.
    let reserve = model.traffic_inset.map_or(BAR_PAD * s, |t| t[0] + TRAFFIC_PAD * s);
    out.hit.push([0.0, 0.0, reserve, sh], HitRegion::Drag);

    // Right side first: the pills have intrinsic widths, the tabs take what
    // remains. Two pills — "(chord) Vertical" and the palette's "(chord)" —
    // 26px tall, hairline border that turns accent under the pointer.
    let pill_y = (sh - PILL_H * s) / 2.0;
    let mut right = m.width - BAR_PAD * s;
    {
        let w = measure(&model.palette_chord, UI_CHORD * s, false, 0.0) + 2.0 * PILL_PAD * s;
        let rect = [right - w, pill_y, w, PILL_H * s];
        let hovered = model.hover == Some(HitRegion::PalettePill);
        pill_button(&mut out.rects, colors, rect, PILL_RADIUS * s, hovered, no_clip);
        out.hit.push(rect, HitRegion::PalettePill);
        out.texts.push(TextRun {
            text: model.palette_chord.clone(),
            pos: [rect[0] + PILL_PAD * s, baseline_in(pill_y, PILL_H * s, UI_CHORD * s)],
            max_width: w,
            color: if hovered { colors.text_active } else { colors.text_inactive },
            clip: no_clip,
            px: UI_CHORD * s,
            bold: false,
            tracking: 0.0,
        });
        right = rect[0] - PILL_GAP * s;
    }
    {
        let label = if model.position == TabsPosition::Top { "Vertical" } else { "Horizontal" };
        let chord_w = measure(&model.toggle_chord, UI_CHORD * s, false, 0.0);
        let label_w = measure(label, UI_SMALL * s, false, 0.0);
        let w = chord_w + PILL_GAP * s + label_w + 2.0 * PILL_PAD * s;
        let rect = [right - w, pill_y, w, PILL_H * s];
        let hovered = model.hover == Some(HitRegion::LayoutPill);
        pill_button(&mut out.rects, colors, rect, PILL_RADIUS * s, hovered, no_clip);
        out.hit.push(rect, HitRegion::LayoutPill);
        let color = if hovered { colors.text_active } else { colors.text_inactive };
        out.texts.push(TextRun {
            text: model.toggle_chord.clone(),
            pos: [rect[0] + PILL_PAD * s, baseline_in(pill_y, PILL_H * s, UI_CHORD * s)],
            max_width: chord_w + 2.0,
            color,
            clip: no_clip,
            px: UI_CHORD * s,
            bold: false,
            tracking: 0.0,
        });
        out.texts.push(TextRun {
            text: label.into(),
            pos: [
                rect[0] + PILL_PAD * s + chord_w + PILL_GAP * s,
                baseline_in(pill_y, PILL_H * s, UI_SMALL * s),
            ],
            max_width: label_w + 2.0,
            color,
            clip: no_clip,
            px: UI_SMALL * s,
            bold: false,
            tracking: 0.0,
        });
        right = rect[0] - BAR_PAD * s;
    }

    let avail = (right - reserve).max(0.0);
    // Chips reach the strip's bottom edge and cover the hairline there, so
    // the active tab's fill meets the pane with nothing drawn between them.
    let clip = [reserve, 0.0, avail, sh];
    let chip_y = sh - TAB_H * s;

    let n = model.tabs.len();
    let gap = TAB_GAP * s;
    let new_tab_w = NEW_TAB_W * s;
    let tab_w = if n == 0 {
        0.0
    } else {
        ((avail - new_tab_w - n as f32 * gap) / n as f32).clamp(TAB_MIN * s, TAB_MAX * s)
    };
    let content_w = n as f32 * (tab_w + gap) + new_tab_w;
    let max_scroll = (content_w - avail).max(0.0);
    out.strip_scroll = model.strip_scroll.clamp(0.0, max_scroll);

    for (i, tab) in model.tabs.iter().enumerate() {
        let x = reserve + i as f32 * (tab_w + gap) - out.strip_scroll;
        let chip = [x, chip_y, tab_w, TAB_H * s];

        let active = i == model.active;
        let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
        if active {
            // Fill + hairline border, rounded on top only. The bottom border
            // and the strip hairline under the chip are then painted out with
            // the fill, which is what "no bottom border" means to an SDF rect
            // whose stroke is a ring.
            out.rects.push(RectInstance {
                radii: [TAB_RADIUS * s, TAB_RADIUS * s, 0.0, 0.0],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(chip, colors.tab_active_bg, clip)
            });
            out.rects.push(RectInstance::filled(
                [x + HAIRLINE * s, sh - HAIRLINE * s, tab_w - 2.0 * HAIRLINE * s, HAIRLINE * s],
                colors.tab_active_bg,
                clip,
            ));
            // The 2px accent rule along the top edge, inset past the corner
            // radius so it never pokes out of the curve.
            out.rects.push(RectInstance::filled(
                [x + TAB_RADIUS * s, chip_y + HAIRLINE * s, tab_w - 2.0 * TAB_RADIUS * s, ACCENT_RULE * s],
                colors.accent,
                clip,
            ));
        } else if hovered {
            out.rects.push(RectInstance {
                radii: [TAB_RADIUS * s, TAB_RADIUS * s, 0.0, 0.0],
                ..RectInstance::filled(chip, colors.tab_hover_bg, clip)
            });
        }
        if let Some(hit) = intersect(chip, clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        // Host dot: the machine's accent on the active tab, faint otherwise.
        let dot_color = if active { host_accent(colors, tab.accent) } else { colors.text_faint };
        dot(
            &mut out.rects,
            x + TAB_PAD * s + DOT * s / 2.0,
            chip_y + TAB_H * s / 2.0,
            DOT * s,
            dot_color,
            clip,
        );

        let mut text_right = x + tab_w - TAB_PAD * s;
        if active || hovered {
            let close_hovered = model.hover == Some(HitRegion::TabClose(tab.addr));
            let close =
                [x + tab_w - TAB_PAD * s - CLOSE * s, chip_y + (TAB_H * s - CLOSE * s) / 2.0, CLOSE * s, CLOSE * s];
            if close_hovered {
                out.rects.push(RectInstance::rounded(close, 4.0 * s, colors.line, clip));
            }
            if let Some(hit) = intersect(close, clip) {
                out.hit.push(hit, HitRegion::TabClose(tab.addr));
            }
            let glyph_w = measure("\u{d7}", UI_BODY * s, false, 0.0);
            out.texts.push(TextRun {
                text: "\u{d7}".into(),
                pos: [
                    close[0] + (close[2] - glyph_w) / 2.0,
                    baseline_in(close[1], close[3], UI_BODY * s),
                ],
                max_width: close[2],
                color: if close_hovered { colors.text_active } else { colors.text_faint },
                clip,
            px: UI_BODY * s,
            bold: false,
            tracking: 0.0,
            });
            text_right = close[0] - TAB_INNER_GAP * s;
        }

        // Two stacked lines: the title, then `host \u{b7} cwd` in mono-small.
        let text_x = x + TAB_PAD * s + DOT * s + TAB_INNER_GAP * s;
        let title_color = match (active, model.focused, tab.connecting) {
            (_, _, true) => colors.text_faint,
            (true, true, _) => colors.text_active,
            _ => colors.text_inactive,
        };
        out.texts.push(TextRun {
            text: tab.title.clone(),
            pos: [text_x, chip_y + 14.5 * s],
            max_width: (text_right - text_x).max(0.0),
            color: title_color,
            clip,
            px: UI_BODY * s,
            bold: false,
            tracking: 0.0,
        });
        // Unreachability is words, not colour alone (#23): the sub-line says
        // it, in warn, where the host's name already lives.
        let (sub, sub_color) = if tab.presence == TabPresence::Unreachable {
            (format!("{} · unreachable", tab.detail()), colors.pill_warn_text)
        } else {
            (tab.detail(), colors.text_faint)
        };
        out.texts.push(TextRun {
            text: sub,
            pos: [text_x, chip_y + 27.0 * s],
            max_width: (text_right - text_x).max(0.0),
            color: sub_color,
            clip,
            px: UI_TAB_SUB * s,
            bold: false,
            tracking: 0.0,
        });
    }

    // The new-tab button trails the last tab and scrolls with the content.
    let nt_x = reserve + n as f32 * (tab_w + gap) - out.strip_scroll;
    let nt = [nt_x, chip_y + (TAB_H * s - NEW_TAB_H * s) / 2.0, NEW_TAB_W * s, NEW_TAB_H * s];
    if model.hover == Some(HitRegion::NewTab) {
        out.rects.push(RectInstance::rounded(nt, PILL_RADIUS * s, colors.tab_hover_bg, clip));
    }
    if let Some(hit) = intersect(nt, clip) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    let plus_w = measure("+", 16.0 * s, false, 0.0);
    out.texts.push(TextRun {
        text: "+".into(),
        pos: [nt[0] + (nt[2] - plus_w) / 2.0, baseline_in(nt[1], nt[3], 16.0 * s)],
        max_width: nt[2],
        color: colors.text_inactive,
        clip,
        px: 16.0 * s,
        bold: false,
            tracking: 0.0,
    });

    // Whatever the content does not cover is a drag handle, like any titlebar.
    let drag_from = (nt[0] + nt[2] + 2.0 * s).min(right);
    if drag_from < right {
        out.hit.push([drag_from, 0.0, right - drag_from, sh], HitRegion::Drag);
    }

    status_bar(model, colors, m, measure, 0.0, &mut out);
    out
}

/// A bordered pill button; the border answers hover, the fill stays absent.
fn pill_button(
    rects: &mut Vec<RectInstance>,
    colors: &ChromeColors,
    rect: [f32; 4],
    radius: f32,
    hovered: bool,
    clip: [f32; 4],
) {
    rects.push(RectInstance {
        radii: [radius; 4],
        border: if hovered { colors.accent } else { colors.line },
        border_width: HAIRLINE * rect[3] / PILL_H, // 1px at the pill's own scale
        ..RectInstance::filled(rect, LinearRgba::TRANSPARENT, clip)
    });
}

/// The 28px status bar (design screen 1): cwd, branch and block count on the
/// left; theme and the link segment on the right. `x0` is where the bar
/// starts — 0 under a top strip, the sidebar's edge in the vertical layout.
fn status_bar(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    x0: f32,
    out: &mut ChromeLayout,
) {
    let Some(st) = model.status.as_ref() else { return };
    let s = m.scale;
    let h = STATUS_H * s;
    let y = m.height - h;
    let no_clip = [0.0, 0.0, m.width, m.height];
    let bar = [x0, y, m.width - x0, h];
    out.rects.push(RectInstance::filled(bar, colors.strip_bg, no_clip));
    out.rects.push(RectInstance::filled(
        [x0, y, m.width - x0, HAIRLINE * s],
        colors.hairline_soft,
        no_clip,
    ));
    out.hit.push(bar, HitRegion::Status);

    let px = UI_STATUS * s;
    let base = baseline_in(y, h, px);

    // Right side first, so the left knows where it must stop.
    let (link_text, link_color) = match st.link {
        super::model::LinkKind::Loopback => ("\u{25cf} loopback", colors.success),
        super::model::LinkKind::Lan => ("\u{25cf} LAN direct", colors.success),
        super::model::LinkKind::Tunnel => ("\u{25cf} tunnel", colors.warn),
        super::model::LinkKind::Stalled => ("\u{25cf} buffering", colors.warn),
        super::model::LinkKind::Reconnecting => ("\u{25cf} reconnecting", colors.danger),
    };
    let latency = st.latency_ms.map(format_ms);
    let mut right_runs: Vec<(String, LinearRgba)> =
        vec![(st.theme.clone(), colors.text_faint), (" \u{b7} ".into(), colors.text_faint)];
    right_runs.push((link_text.into(), link_color));
    if let Some(ms) = latency {
        right_runs.push((format!(" {ms}"), colors.text_faint));
    }
    let right_w: f32 = right_runs.iter().map(|(t, _)| measure(t, px, false, 0.0)).sum();
    let mut x = m.width - STATUS_HPAD * s - right_w;
    let right_start = x;
    for (text, color) in right_runs {
        let w = measure(&text, px, false, 0.0);
        out.texts.push(TextRun {
            text,
            pos: [x, base],
            max_width: w + 2.0,
            color,
            clip: no_clip,
            px,
            bold: false,
            tracking: 0.0,
        });
        x += w;
    }

    let mut left_runs: Vec<(String, LinearRgba)> =
        vec![(st.cwd.clone(), colors.text_inactive)];
    if let Some(b) = &st.branch {
        left_runs.push((" \u{b7} ".into(), colors.text_faint));
        left_runs.push((format!("\u{2387} {b}"), colors.success));
    }
    left_runs.push((" \u{b7} ".into(), colors.text_faint));
    let blocks = if st.blocks == 1 { "1 block".into() } else { format!("{} blocks", st.blocks) };
    left_runs.push((blocks, colors.text_faint));

    let mut x = x0 + STATUS_HPAD * s;
    let stop = right_start - TEXT_PAD * s;
    for (text, color) in left_runs {
        if x >= stop {
            break;
        }
        let w = measure(&text, px, false, 0.0).min(stop - x);
        out.texts.push(TextRun {
            text,
            pos: [x, base],
            max_width: w,
            color,
            clip: no_clip,
            px,
            bold: false,
            tracking: 0.0,
        });
        x += w;
    }
}

/// "0.08 ms", "0.3 ms", "41 ms" — the design's precision: enough digits to
/// be honest, never trailing noise.
pub fn format_ms(ms: f32) -> String {
    if ms < 0.1 {
        format!("{ms:.2} ms")
    } else if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{} ms", ms.round() as i64)
    }
}

fn vertical(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
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
    let header_h = model.traffic_inset.map_or(SIDEBAR_HEADER * s, |t| t[1].max(SIDEBAR_HEADER * s));
    out.hit.push([0.0, 0.0, sw, header_h], HitRegion::Drag);

    // The search affordance: looks like an input, acts like a button.
    let search = [
        SEARCH_PAD * s,
        header_h + 0.0,
        sw - 2.0 * SEARCH_PAD * s,
        SEARCH_H * s,
    ];
    {
        let hovered = model.hover == Some(HitRegion::SidebarSearch);
        out.rects.push(RectInstance {
            radii: [PILL_RADIUS * s; 4],
            border: if hovered { colors.accent } else { colors.line },
            border_width: HAIRLINE * s,
            ..RectInstance::filled(search, colors.panel_bg, no_clip)
        });
        out.hit.push(search, HitRegion::SidebarSearch);
        let mut x = search[0] + SEARCH_PAD * s;
        let chord_w = measure(&model.palette_chord, UI_SMALL * s, false, 0.0);
        out.texts.push(TextRun {
            text: model.palette_chord.clone(),
            pos: [x, baseline_in(search[1], search[3], UI_SMALL * s)],
            max_width: chord_w + 2.0,
            color: colors.text_faint,
            clip: no_clip,
            px: UI_SMALL * s,
            bold: false,
            tracking: 0.0,
        });
        x += chord_w + TEXT_PAD * s;
        out.texts.push(TextRun {
            text: "Search sessions, blocks, hosts".into(),
            pos: [x, baseline_in(search[1], search[3], 12.0 * s)],
            max_width: (search[0] + search[2] - SEARCH_PAD * s - x).max(0.0),
            color: colors.text_faint,
            clip: no_clip,
            px: 12.0 * s,
            bold: false,
            tracking: 0.0,
        });
    }

    let footer_h = FOOTER_H * s;
    let rows_top = search[1] + search[3] + SEARCH_PAD * s;
    let rows_clip = [0.0, rows_top, sw, (m.height - rows_top - footer_h).max(0.0)];

    // Geometry per group: a header line, then its session rows.
    let group_header_h = GROUP_HEADER_H * s;
    let row_h = SIDE_ROW_H * s;

    // With no sidebar model (an early frame), group by the tabs' own host
    // fields — the machine's name must appear in words whatever arrives
    // first (#23).
    let fallback: Vec<super::model::HostGroup> = {
        let mut gs: Vec<super::model::HostGroup> = Vec::new();
        for (i, t) in model.tabs.iter().enumerate() {
            if let Some(g) = gs.iter_mut().find(|g| g.label == t.host) {
                g.tabs.push(i);
            } else {
                gs.push(super::model::HostGroup {
                    label: t.host.clone(),
                    accent: t.accent,
                    sub: String::new(),
                    online: t.presence == TabPresence::Online,
                    tabs: vec![i],
                });
            }
        }
        gs
    };
    let groups: &[super::model::HostGroup] =
        model.sidebar.as_ref().map_or(&fallback[..], |sb| &sb.groups[..]);

    let content_h: f32 = groups
        .iter()
        .map(|g| group_header_h + g.tabs.len() as f32 * row_h + GROUP_GAP * s)
        .sum::<f32>()
        + row_h; // the trailing new-tab row
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    out.strip_scroll = model.strip_scroll.clamp(0.0, max_scroll);

    let mut y = rows_top - out.strip_scroll;
    for group in groups {
        if !group.label.is_empty() {
            // Group header: accent dot, uppercase tracked label, mono sub.
            let cy = y + group_header_h / 2.0;
            let dot_color = if group.online {
                host_accent(colors, group.accent)
            } else {
                colors.text_faint
            };
            dot(&mut out.rects, ROW_HPAD * s + 3.0 * s + DOT * s / 2.0, cy, DOT * s, dot_color, rows_clip);
            let label = group.label.to_uppercase();
            let label_px = UI_STATUS * s;
            let tracking = 0.09 * label_px;
            let label_w = measure(&label, label_px, true, tracking);
            let label_x = ROW_HPAD * s + 3.0 * s + DOT * s + 7.0 * s;
            out.texts.push(TextRun {
                text: label,
                pos: [label_x, baseline_in(y, group_header_h, label_px)],
                max_width: label_w + 2.0,
                color: colors.text_inactive,
                clip: rows_clip,
                px: label_px,
                bold: true,
                tracking,
            });
            if !group.sub.is_empty() {
                let sub_px = UI_CHORD * s;
                out.texts.push(TextRun {
                    text: group.sub.clone(),
                    pos: [label_x + label_w + 7.0 * s, baseline_in(y, group_header_h, sub_px)],
                    max_width: (sw - label_x - label_w - 14.0 * s).max(0.0),
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: sub_px,
                    bold: false,
                    tracking: 0.0,
                });
            }
        }
        y += if group.label.is_empty() { 0.0 } else { group_header_h };

        for &ti in &group.tabs {
            let Some(tab) = model.tabs.get(ti) else { continue };
            let row = [ROW_HPAD * s, y, sw - 2.0 * ROW_HPAD * s, row_h];

            let active = ti == model.active;
            let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
            if active {
                out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.accent_soft, rows_clip));
            } else if hovered {
                out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.tab_hover_bg, rows_clip));
            }
            if let Some(hit) = intersect(row, rows_clip) {
                out.hit.push(hit, HitRegion::Tab(tab.addr));
            }

            // 5px state dot: running pulses warn, the live (active) session
            // is success, an idle one faint.
            let dot_d = 5.0 * s;
            let dot_color = if tab.running {
                colors.warn
            } else if active {
                colors.success
            } else {
                colors.text_faint
            };
            dot(&mut out.rects, row[0] + 8.0 * s + dot_d / 2.0, y + row_h / 2.0, dot_d, dot_color, rows_clip);

            let text_x = row[0] + 8.0 * s + dot_d + TAB_INNER_GAP * s;
            let mut text_right = row[0] + row[2] - 8.0 * s;

            if !tab.age.is_empty() {
                let age_px = UI_CHORD * s;
                let age_w = measure(&tab.age, age_px, false, 0.0);
                out.texts.push(TextRun {
                    text: tab.age.clone(),
                    pos: [text_right - age_w, baseline_in(y, row_h, age_px)],
                    max_width: age_w + 2.0,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: age_px,
                    bold: false,
                    tracking: 0.0,
                });
                text_right -= age_w + TEXT_PAD * s;
            }

            // Unreachability in words, here as everywhere (#23).
            let title = if tab.presence == TabPresence::Unreachable {
                format!("{} · unreachable", tab.title)
            } else {
                tab.title.clone()
            };
            let title_color = match (active, tab.connecting) {
                (_, true) => colors.text_faint,
                (true, false) => colors.text_active,
                _ => colors.text_inactive,
            };
            out.texts.push(TextRun {
                text: title,
                pos: [text_x, y + 17.0 * s],
                max_width: (text_right - text_x).max(0.0),
                color: title_color,
                clip: rows_clip,
                px: UI_BODY * s,
                bold: false,
                tracking: 0.0,
            });
            out.texts.push(TextRun {
                text: tab.cwd.clone(),
                pos: [text_x, y + 31.0 * s],
                max_width: (text_right - text_x).max(0.0),
                color: colors.text_faint,
                clip: rows_clip,
                px: UI_STATUS * s,
                bold: false,
                tracking: 0.0,
            });
            y += row_h;
        }
        y += GROUP_GAP * s;
    }

    // New-tab row, trailing the last group.
    let row = [ROW_HPAD * s, y, sw - 2.0 * ROW_HPAD * s, row_h];
    if model.hover == Some(HitRegion::NewTab) {
        out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.tab_hover_bg, rows_clip));
    }
    if let Some(hit) = intersect(row, rows_clip) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    out.texts.push(TextRun {
        text: "+ new session".into(),
        pos: [row[0] + 8.0 * s, baseline_in(y, row_h, UI_BODY * s)],
        max_width: row[2] - 16.0 * s,
        color: colors.text_inactive,
        clip: rows_clip,
        px: UI_BODY * s,
        bold: false,
        tracking: 0.0,
    });

    // Footer: fleet counts, and the door to the fleet view.
    {
        let fy = m.height - footer_h;
        let footer = [0.0, fy, sw, footer_h];
        let hovered = model.hover == Some(HitRegion::FleetFooter);
        if hovered {
            out.rects.push(RectInstance::filled(footer, colors.tab_hover_bg, no_clip));
        }
        out.rects.push(RectInstance::filled(
            [0.0, fy, sw, HAIRLINE * s],
            colors.hairline_soft,
            no_clip,
        ));
        out.hit.push(footer, HitRegion::FleetFooter);
        let (online, asleep) = model
            .sidebar
            .as_ref()
            .map_or((1, 0), |sb| (sb.hosts_online, sb.hosts_asleep));
        let text = if asleep > 0 {
            format!("{online} hosts online · {asleep} asleep")
        } else if online == 1 {
            "1 host online".to_string()
        } else {
            format!("{online} hosts online")
        };
        dot(&mut out.rects, BAR_PAD * s + 3.0 * s, fy + footer_h / 2.0, DOT * s, colors.success, no_clip);
        out.texts.push(TextRun {
            text,
            pos: [BAR_PAD * s + DOT * s + TEXT_PAD * s, baseline_in(fy, footer_h, 11.5 * s)],
            max_width: sw - 30.0 * s,
            color: colors.text_inactive,
            clip: no_clip,
            px: 11.5 * s,
            bold: false,
            tracking: 0.0,
        });
    }

    // The slim title bar over the main column: session name, cwd, host chip,
    // and the way back to horizontal tabs.
    {
        let bar = [sw, 0.0, m.width - sw, SLIM_BAR_H * s];
        out.rects.push(RectInstance::filled(bar, colors.strip_bg, no_clip));
        out.rects.push(RectInstance::filled(
            [sw, SLIM_BAR_H * s - HAIRLINE * s, m.width - sw, HAIRLINE * s],
            colors.line,
            no_clip,
        ));
        out.hit.push(bar, HitRegion::Drag);

        if let Some(tab) = model.tabs.get(model.active) {
            let mut x = sw + SLIM_PAD * s;
            let name_px = 13.0 * s;
            let name_w = measure(&tab.title, name_px, false, 0.0).min(bar[2] * 0.4);
            out.texts.push(TextRun {
                text: tab.title.clone(),
                pos: [x, baseline_in(0.0, bar[3], name_px)],
                max_width: name_w,
                color: colors.text_active,
                clip: no_clip,
                px: name_px,
                bold: false,
                tracking: 0.0,
            });
            x += name_w + 10.0 * s;
            if !tab.cwd.is_empty() {
                let cwd_w = measure(&tab.cwd, UI_SMALL * s, false, 0.0).min(bar[2] * 0.3);
                out.texts.push(TextRun {
                    text: tab.cwd.clone(),
                    pos: [x, baseline_in(0.0, bar[3], UI_SMALL * s)],
                    max_width: cwd_w,
                    color: colors.text_inactive,
                    clip: no_clip,
                    px: UI_SMALL * s,
                    bold: false,
                    tracking: 0.0,
                });
                x += cwd_w + 10.0 * s;
            }
            // The host chip: where this shell runs, said in a pill.
            let chip_px = UI_STATUS * s;
            let chip_text_w = measure(&tab.host, chip_px, false, 0.0);
            let chip_h = 20.0 * s;
            let chip = [x, (bar[3] - chip_h) / 2.0, 5.0 * s + 6.0 * s + chip_text_w + 16.0 * s, chip_h];
            out.rects.push(RectInstance::rounded(chip, 6.0 * s, colors.accent_soft, no_clip));
            dot(
                &mut out.rects,
                chip[0] + 8.0 * s + 2.5 * s,
                chip[1] + chip_h / 2.0,
                5.0 * s,
                colors.success,
                no_clip,
            );
            out.texts.push(TextRun {
                text: tab.host.clone(),
                pos: [chip[0] + 8.0 * s + 5.0 * s + 6.0 * s, baseline_in(chip[1], chip_h, chip_px)],
                max_width: chip_text_w + 2.0,
                color: colors.accent,
                clip: no_clip,
                px: chip_px,
                bold: false,
                tracking: 0.0,
            });
        }

        // The way back: same pill, same region, other word.
        let label = "Horizontal tabs";
        let label_w = measure(label, UI_SMALL * s, false, 0.0);
        let w = label_w + 2.0 * PILL_PAD * s;
        let rect = [m.width - BAR_PAD * s - w, (bar[3] - PILL_H * s) / 2.0, w, PILL_H * s];
        let hovered = model.hover == Some(HitRegion::LayoutPill);
        pill_button(&mut out.rects, colors, rect, PILL_RADIUS * s, hovered, no_clip);
        out.hit.push(rect, HitRegion::LayoutPill);
        out.texts.push(TextRun {
            text: label.into(),
            pos: [rect[0] + PILL_PAD * s, baseline_in(rect[1], rect[3], UI_SMALL * s)],
            max_width: label_w + 2.0,
            color: if hovered { colors.text_active } else { colors.text_inactive },
            clip: no_clip,
            px: UI_SMALL * s,
            bold: false,
            tracking: 0.0,
        });
    }

    // The status bar spans the main column only; the sidebar keeps its full
    // height (design screen 2).
    status_bar(model, colors, m, measure, sw, &mut out);
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
        // The detail line carries the host's name exactly as the app composes
        // it, so the words-not-colour tests exercise the real shape.
        let host = match &origin {
            TabOrigin::Remote { host_label } => host_label.clone(),
            TabOrigin::Local => "local".into(),
        };
        TabModel {
            addr: addr(n),
            title: format!("tab {n}"),
            host,
            cwd: format!("~/dir{n}"),
            origin,
            presence,
            accent: usize::from(n),
            running: false,
            age: "2m".into(),
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
            font_px: GRID_PX * scale,
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
            status: Some(super::super::model::StatusModel {
                cwd: "~/dev/zesterm".into(),
                branch: Some("main".into()),
                blocks: 3,
                theme: "obsidian".into(),
                link: super::super::model::LinkKind::Lan,
                latency_ms: Some(0.3),
            }),
            sidebar: None,
            screen: None,
            grid_area: [0.0, 46.0, 1200.0, 726.0],
            toggle_chord: "⌘⇧E".into(),
            palette_chord: "⌘K".into(),
            picker: None,
            palette: None,
            settings: None,
        }
    }

    /// Eight pixels a character at the grid size, scaled linearly for other
    /// sizes: enough for the tests to reason about truncation and the type
    /// scale without a font.
    fn measure(s: &str, px: f32, _bold: bool, tracking: f32) -> f32 {
        s.chars().count() as f32 * (8.0 * (px / GRID_PX) + tracking)
    }

    /// The grid font size the test metrics report.
    const GRID_PX: f32 = 13.0;

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
        // Below the header: the search affordance, then the first session
        // row — both reachable (design screen 2).
        let search_found = (0..262).step_by(2).any(|x| {
            (0..200).step_by(2).any(|y| {
                l.hit.hit(x as f32, y as f32) == Some(HitRegion::SidebarSearch)
            })
        });
        assert!(search_found, "the search affordance must be clickable");
        let row_found = (0..262).step_by(2).any(|x| {
            (0..400).step_by(2).any(|y| {
                l.hit.hit(x as f32, y as f32) == Some(HitRegion::Tab(addr(1)))
            })
        });
        assert!(row_found, "the first session row sits below the search box");
        let footer_found = (0..262).step_by(2).any(|x| {
            (600..800).step_by(2).any(|y| {
                l.hit.hit(x as f32, y as f32) == Some(HitRegion::FleetFooter)
            })
        });
        assert!(footer_found, "the fleet footer owns the sidebar's bottom");
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
            // The host may be spelled on the chip's sub-line (Top) or as an
            // uppercase group label (Left); either way it is *text*, and the
            // unreachability is said in words somewhere.
            let says_host =
                l.texts.iter().any(|t| t.text.to_lowercase().contains("alien"));
            let says_unreachable = l.texts.iter().any(|t| t.text.contains("unreachable"));
            assert!(
                says_host && says_unreachable,
                "{position:?} must spell out host and unreachability"
            );
        }
    }

    #[test]
    fn the_title_bar_carries_the_pills_and_the_status_bar_the_bottom() {
        // Design screen 1: the layout/palette pills are clickable in the top
        // strip, and the status bar owns exactly its 28 logical pixels — one
        // pixel above it belongs to the grid, or the bar is eating a row.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);

        let strip_has = |want: HitRegion| {
            (0..1200)
                .step_by(2)
                .any(|x| (0..46).step_by(2).any(|y| l.hit.hit(x as f32, y as f32) == Some(want)))
        };
        assert!(strip_has(HitRegion::LayoutPill), "the layout toggle must be clickable");
        assert!(strip_has(HitRegion::PalettePill), "the palette pill must be clickable");

        assert_eq!(
            l.hit.hit(600.0, 800.0 - STATUS_H / 2.0),
            Some(HitRegion::Status),
            "the status bar swallows its own clicks"
        );
        assert_eq!(
            l.hit.hit(600.0, 800.0 - STATUS_H - 1.0),
            None,
            "one pixel above the bar is the grid's"
        );
        assert!(
            l.texts.iter().any(|t| t.text.contains("LAN direct")),
            "the link segment says its path in words"
        );
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
                PickerRow::Group { title: "Blocks".into() },
                PickerRow::Block {
                    command: "cargo build --workspace".into(),
                    provenance: "studio · 2m ago · exit 0".into(),
                    ok: true,
                },
                PickerRow::Group { title: "Sessions".into() },
                PickerRow::Session {
                    title: "vim".into(),
                    detail: "~/dev".into(),
                    host: "andy-mac".into(),
                    attached: false,
                    attached_here: false,
                },
                PickerRow::Host {
                    label: "andy-mac".into(),
                    presence: TabPresence::Online,
                    detail: "LAN · 0.3 ms".into(),
                },
                PickerRow::Action { name: "New tab".into(), chord: "⌘T".into() },
            ],
            selected: 1,
            filter: String::new(),
            scroll: 0.0,
            ensure_visible: false,
            hosts_searched: 4,
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
                    // The panel between rows swallows; group labels are part
                    // of it, not rows.
                    Some(HitRegion::PickerPanel) => {}
                    Some(other) => {
                        panic!("a click at ({x},{y}) escaped the picker: {other:?}")
                    }
                    None => panic!("({x},{y}) hit nothing; the scrim must cover the window"),
                }
            }
        }
        assert_eq!(
            seen_rows,
            [1usize, 3, 4, 5].into(),
            "every actionable row must be clickable, and group labels must not be"
        );
        assert!(scrim_hits > 0, "the scrim must be reachable around the panel");
    }

    fn palette_rows(n: usize) -> Vec<crate::chrome::model::PaletteRow> {
        use crate::chrome::model::PaletteRow;
        let mut rows = vec![PaletteRow::Group { title: "Tabs".into() }];
        rows.extend((0..n).map(|i| PaletteRow::Command {
            name: format!("Command {i}"),
            chord: "⌘X".into(),
            runnable: true,
        }));
        rows.push(PaletteRow::Command {
            name: "a reference row".into(),
            chord: String::new(),
            runnable: false,
        });
        rows
    }

    #[test]
    fn the_palette_is_modal_and_only_runnable_rows_answer() {
        // Same definition of modal as the picker test above: every point in
        // the window answers as the palette's rows, panel or scrim, and a
        // click can never reach a tab or the grid while it is up. Reference
        // rows must NOT answer as rows — a click on one runs nothing, so it
        // must land on the panel.
        use crate::chrome::model::PaletteModel;
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.palette = Some(PaletteModel {
            rows: palette_rows(2),
            selected: 1,
            filter: String::new(),
            scroll: 0.0,
            ensure_visible: false,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);

        let mut seen_rows = std::collections::HashSet::new();
        let mut panel_hits = 0u32;
        let mut scrim_hits = 0u32;
        for x in (0..1200).step_by(4) {
            for y in (0..800).step_by(4) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::PaletteRow(i)) => {
                        seen_rows.insert(i);
                    }
                    Some(HitRegion::PalettePanel) => panel_hits += 1,
                    Some(HitRegion::PaletteScrim) => scrim_hits += 1,
                    Some(other) => panic!("a click at ({x},{y}) escaped the palette: {other:?}"),
                    None => panic!("({x},{y}) hit nothing; the scrim must cover the window"),
                }
            }
        }
        assert_eq!(
            seen_rows,
            [1usize, 2].into(),
            "the runnable commands answer; the header (0) and reference row (3) must not"
        );
        assert!(panel_hits > 0, "the panel must swallow clicks between rows");
        assert!(scrim_hits > 0, "the scrim must be reachable around the panel");
    }

    #[test]
    fn palette_navigation_never_acts_on_an_offscreen_row() {
        // Forty rows overflow the panel: a wild scroll clamps, and with the
        // selection at the end ensure_visible must move the view so the
        // selected row is hittable — Enter must never run something the
        // user cannot see.
        use crate::chrome::model::PaletteModel;
        let m = metrics(1200.0, 800.0, 1.0);
        let rows = palette_rows(40);
        let selected = 40; // the last runnable command
        let mut mo = model(Vec::new(), TabsPosition::Top);
        mo.palette = Some(PaletteModel {
            rows,
            selected,
            filter: String::new(),
            scroll: 1e9,
            ensure_visible: true,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.palette_scroll > 0.0, "an overflowing palette scrolls");
        assert!(l.palette_scroll < 1e9, "and the scroll is clamped to the content");
        let found = (0..1200).step_by(4).any(|x| {
            (0..800)
                .step_by(4)
                .any(|y| l.hit.hit(x as f32, y as f32) == Some(HitRegion::PaletteRow(selected)))
        });
        assert!(found, "the selected command must be visible and hittable after ensure_visible");
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
    fn a_drawn_slider_reports_the_track_it_drew() {
        // The click-to-set fraction is computed against `settings_tracks`;
        // if the reported rect and the drawn one could differ, the pointer
        // would set values the pixels never showed.
        use crate::chrome::model::{SettingsModel, SettingsRowModel, SettingsValueCell};
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(Vec::new(), TabsPosition::Top);
        mo.settings = Some(SettingsModel {
            rows: vec![
                SettingsRowModel::Group { title: "Window".into() },
                SettingsRowModel::Setting {
                    label: "Opacity".into(),
                    key: "window.opacity".into(),
                    description: "background opacity".into(),
                    value: SettingsValueCell::Slider { frac: 0.5, text: "0.5".into() },
                    provenance: None,
                    restart: false,
                    inert: false,
                    modified: false,
                },
            ],
            selected: 1,
            filter: String::new(),
            scroll: 0.0,
            ensure_visible: false,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);
        let (row, track) =
            *l.settings_tracks.first().expect("the slider row must report its track");
        assert_eq!(row, 1);
        // The centre of the reported track must answer as that slider.
        let (cx, cy) = (track[0] + track[2] / 2.0, track[1] + track[3] / 2.0);
        assert_eq!(
            l.hit.hit(cx, cy),
            Some(HitRegion::SettingsSlider(1)),
            "the grab band must cover the track it reports"
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
