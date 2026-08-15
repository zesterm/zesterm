//! A block's ⋯ menu, laid out (design §3).
//!
//! Its own module rather than a reuse of `settings_screen::dropdown_menu` or
//! `layout::launcher_overlay`: each of those is welded to its own model and
//! pushes its own hit regions, and routing a block action through the settings
//! dispatch to borrow a panel would be the wrong kind of saving. What *is*
//! borrowed is the part that was expensive to get right in each — the
//! flip-above clamp from the dropdown, and the transparent full-window scrim
//! from the launcher.
//!
//! # Why being in the cached layout is what makes the menu win
//!
//! `App::chrome_hit` consults the cached chrome layout first and the per-frame
//! block hit map only `.or_else(…)`. So a menu emitted here, with a scrim over
//! the whole window, outranks every block header and rail with no ranking code
//! anywhere — and while it is open no pointer event can reach the grid.

use zest_render_wgpu::RectInstance;

use super::hit::HitRegion;
use super::layout::{baseline_in, ChromeLayout, TextRun, HAIRLINE};
use super::model::{BlockMenuModel, BlockMenuRow, ChromeMetrics};
use super::theme::ChromeColors;

// Logical px — the launcher's vocabulary at menu scale.
const W: f32 = 236.0;
const RADIUS: f32 = 10.0;
const PAD: f32 = 6.0;
const ROW_H: f32 = 30.0;
const DIVIDER_H: f32 = 9.0;
const HPAD: f32 = 10.0;
const LABEL_PX: f32 = 12.5;
const CHORD_PX: f32 = 11.0;
const ROW_RADIUS: f32 = 7.0;
/// Gap between the anchor and the panel.
const DROP: f32 = 6.0;
/// The panel never touches the pane's edge.
const MARGIN: f32 = 8.0;

pub(super) fn block_menu_overlay(
    menu: &BlockMenuModel,
    hover: Option<HitRegion>,
    area: [f32; 4],
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    // Transparent and full-window, like the launcher's: a menu dims nothing,
    // but a click-away must dismiss without the press falling through to the
    // grid, a header, or a rail. A hit region with no rect is exactly that.
    out.hit.push(no_clip, HitRegion::BlockMenuScrim);

    let row_h = |row: &BlockMenuRow| match row {
        BlockMenuRow::Action { .. } => ROW_H * s,
        BlockMenuRow::Divider => DIVIDER_H * s,
    };
    let w = (W * s).min((area[2] - 2.0 * MARGIN * s).max(0.0));
    let h = menu.rows.iter().map(&row_h).sum::<f32>() + 2.0 * PAD * s;

    // Below the anchor, flipped above when it would leave the pane — the
    // dropdown's clamp, which matters more here than there: a block's ⋯ is a
    // grid row, so it is frequently near the bottom.
    let anchor = menu.anchor;
    let below = anchor[1] + anchor[3] + DROP * s + h <= area[1] + area[3] - MARGIN * s;
    let y = if below {
        anchor[1] + anchor[3] + DROP * s
    } else {
        (anchor[1] - h - DROP * s).max(area[1] + MARGIN * s)
    };
    // Right-aligned on the anchor, since the ⋯ sits at the header's right end.
    let x = (anchor[0] + anchor[2] - w)
        .clamp(area[0] + MARGIN * s, (area[0] + area[2] - w - MARGIN * s).max(area[0] + MARGIN * s));
    let panel = [x, y, w, h];

    out.rects.push(RectInstance {
        radii: [RADIUS * s; 4],
        border: colors.line,
        border_width: HAIRLINE * s,
        shadow_blur: 20.0 * s,
        shadow_alpha: colors.shadow_alpha,
        ..RectInstance::filled(panel, colors.panel_bg, no_clip)
    });
    // Pushed before the rows, so each row wins where they overlap and what is
    // left — the padding, the dividers — belongs to the panel and swallows.
    out.hit.push(panel, HitRegion::BlockMenuPanel);

    let mut ry = panel[1] + PAD * s;
    for (i, row) in menu.rows.iter().enumerate() {
        let hr = row_h(row);
        match row {
            BlockMenuRow::Divider => {
                out.rects.push(RectInstance::filled(
                    [panel[0] + HPAD * s, ry + hr / 2.0, w - 2.0 * HPAD * s, HAIRLINE * s],
                    colors.line,
                    no_clip,
                ));
            }
            BlockMenuRow::Action { label, chord, enabled } => {
                let rect = [panel[0] + PAD * s, ry, w - 2.0 * PAD * s, hr];
                // A disabled row is drawn and takes no clicks: it falls
                // through to the panel, which swallows. An affordance that
                // answers a click by doing nothing is worse than none.
                if *enabled {
                    out.hit.push(rect, HitRegion::BlockMenuRow(i));
                    let fill = if menu.selected == i {
                        Some(colors.accent_soft)
                    } else if hover == Some(HitRegion::BlockMenuRow(i)) {
                        Some(colors.tab_hover_bg)
                    } else {
                        None
                    };
                    if let Some(fill) = fill {
                        out.rects.push(RectInstance::rounded(
                            rect,
                            ROW_RADIUS * s,
                            fill,
                            no_clip,
                        ));
                    }
                }
                out.texts.push(TextRun {
                    text: label.clone(),
                    pos: [rect[0] + HPAD * s, baseline_in(ry, hr, LABEL_PX * s)],
                    max_width: rect[2] - 2.0 * HPAD * s,
                    color: if *enabled { colors.text_active } else { colors.text_faint },
                    clip: no_clip,
                    px: LABEL_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                if !chord.is_empty() {
                    let cw = measure(chord, CHORD_PX * s, false, 0.0);
                    out.texts.push(TextRun {
                        text: chord.clone(),
                        pos: [
                            rect[0] + rect[2] - HPAD * s - cw,
                            baseline_in(ry, hr, CHORD_PX * s),
                        ],
                        max_width: cw + 2.0,
                        color: colors.text_faint,
                        clip: no_clip,
                        px: CHORD_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }
            }
        }
        ry += hr;
    }
}
