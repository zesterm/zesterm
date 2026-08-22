//! The live prompt's context chips (#420) — branch, cwd, venv, the last
//! failure — drawn as chrome beside where the user is about to type.
//!
//! Like [`super::blocks`], this rides the scrollback: rebuilt per frame from
//! an app-extracted view, pure, and testable with a measure closure. What it
//! draws comes from `SessionInfo.context` — the *session's own daemon's*
//! word — so a browser attached to a machine across the relay renders the
//! same chips this window does.
//!
//! Placement honours the never-overlay rule by restating what it protects:
//! never over typed text or the caret. When the row above the prompt is
//! blank (a prompt that leaves one, and compact-PS1 mode once it exists),
//! the chips take that row, left-aligned, the Warp shape. Otherwise they
//! right-align on the prompt row itself and *disappear* — entirely, not
//! squeezed — the moment typed text plus a margin would reach them: a chip
//! that hides is a chip; a chip that overlaps the command being typed is a
//! rendering bug with a feature's name.

use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::{ChromeHitMap, HitRegion};
use super::layout::TextRun;
use super::theme::ChromeColors;

/// Which chip. Carried in [`HitRegion::PromptChip`], so it stays `Copy`; at
/// most one chip of each kind exists per prompt, which is what lets a click
/// find its value again without the region carrying a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Cwd,
    Git,
    Venv,
    Conda,
    Kube,
    Aws,
    Node,
    Ssh,
    Exit,
}

/// One chip, resolved to what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub kind: ChipKind,
    /// The full value a click copies (the unshortened path, the branch).
    pub value: String,
    /// What the chip shows — shortened cwd, `venv ml`, `exit 3`.
    pub label: String,
}

/// The extracted state: where the live prompt is and what surrounds it.
///
/// Extracted by the app under its terminal lock; `None` at the extraction
/// site — not an empty view — when there is no live prompt block, the alt
/// screen is active, or every chip is filtered off.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptChipsView {
    /// Viewport row of the live prompt's line.
    pub prompt_row: usize,
    /// The row above exists and holds nothing — the chips' preferred home.
    pub row_above_blank: bool,
    /// Columns occupied on the prompt row, caret included: the collision
    /// edge for the on-row fallback.
    pub occupied_cols: usize,
    /// In display order, already filtered by `prompt.widgets`.
    pub chips: Vec<Chip>,
}

/// The per-frame output, appended after the cached chrome like the block
/// headers are.
#[derive(Debug, Default)]
pub struct ChipChrome {
    pub rects: Vec<RectInstance>,
    pub texts: Vec<TextRun>,
    pub hit: ChromeHitMap,
}

// Geometry, logical px. The chips are deliberately one notch quieter than
// the block headers: they sit where the eye already is.
const CHIP_H: f32 = 20.0;
const CHIP_RADIUS: f32 = 6.0;
const CHIP_HPAD: f32 = 8.0;
const CHIP_GAP: f32 = 6.0;
const CHIP_PX: f32 = 11.0;
/// Cells of air between typed text and the on-row chips before they hide.
const COLLIDE_MARGIN_CELLS: f32 = 2.0;

fn baseline_in(y: f32, h: f32, px: f32) -> f32 {
    y + (h + px * 0.72) / 2.0
}

/// The label's tint: state colours for state, identity colours for identity.
fn chip_tint(kind: ChipKind, colors: &ChromeColors) -> LinearRgba {
    match kind {
        ChipKind::Cwd => colors.accent,
        ChipKind::Git => colors.success,
        ChipKind::Venv | ChipKind::Conda | ChipKind::Node => colors.info,
        ChipKind::Kube | ChipKind::Aws | ChipKind::Ssh => colors.warn,
        ChipKind::Exit => colors.danger,
    }
}

/// Lay the chips out over the grid area `area`, rows of `cell_h` px and
/// columns of `cell_w` px. Returns nothing drawn when the view has no chips
/// or the fallback row has no room — hiding is a designed state, not an
/// error.
#[allow(clippy::too_many_arguments, reason = "one call site; a params struct would name nothing")]
pub fn layout_prompt_chips(
    view: &PromptChipsView,
    area: [f32; 4],
    cell_w: f32,
    cell_h: f32,
    scale: f32,
    colors: &ChromeColors,
    hover: Option<HitRegion>,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> ChipChrome {
    let mut out = ChipChrome::default();
    if view.chips.is_empty() {
        return out;
    }

    let px = CHIP_PX * scale;
    let chip_h = (CHIP_H * scale).min(cell_h);
    let hpad = CHIP_HPAD * scale;
    let gap = CHIP_GAP * scale;

    // Width of every chip, before deciding where — the on-row fallback needs
    // the total to know whether it fits at all.
    let widths: Vec<f32> =
        view.chips.iter().map(|c| measure(&c.label, px, false, 0.0) + hpad * 2.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + gap * (widths.len() as f32 - 1.0);

    let row_h = cell_h;
    let (mut x, y) = if view.row_above_blank && view.prompt_row > 0 {
        // The Warp shape: the blank line above the prompt, left-aligned.
        (area[0], area[1] + (view.prompt_row - 1) as f32 * row_h)
    } else {
        // On the prompt row, pushed to the right edge — gone entirely once
        // typed text plus the margin would reach the row of chips.
        let text_edge =
            area[0] + (view.occupied_cols as f32 + COLLIDE_MARGIN_CELLS) * cell_w;
        let x = area[0] + area[2] - total;
        if x < text_edge {
            return out;
        }
        (x, area[1] + view.prompt_row as f32 * row_h)
    };
    // Off the visible grid (a prompt scrolled out while its chips were being
    // asked for): nothing to draw.
    if y < area[1] || y + row_h > area[1] + area[3] {
        return out;
    }

    let chip_y = y + (row_h - chip_h) / 2.0;
    let clip = area;
    for (chip, w) in view.chips.iter().zip(&widths) {
        let rect = [x, chip_y, *w, chip_h];
        let hovered = hover == Some(HitRegion::PromptChip(chip.kind));
        out.rects.push(RectInstance {
            radii: [CHIP_RADIUS * scale; 4],
            border: if hovered { colors.accent } else { colors.line },
            border_width: scale.max(1.0),
            ..RectInstance::filled(rect, colors.pill_bg, clip)
        });
        out.texts.push(TextRun {
            text: chip.label.clone(),
            pos: [x + hpad, baseline_in(chip_y, chip_h, px)],
            max_width: *w - hpad * 2.0,
            color: chip_tint(chip.kind, colors),
            clip,
            px,
            bold: false,
            tracking: 0.0,
        });
        out.hit.push(rect, HitRegion::PromptChip(chip.kind));
        x += w + gap;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0)
    }

    /// Arithmetic measure: 6px per character at 11px, scaled linearly.
    fn measure(text: &str, px: f32, _bold: bool, _tracking: f32) -> f32 {
        text.chars().count() as f32 * px * 0.55
    }

    fn chip(kind: ChipKind, label: &str) -> Chip {
        Chip { kind, value: label.to_string(), label: label.to_string() }
    }

    fn view(chips: Vec<Chip>) -> PromptChipsView {
        PromptChipsView { prompt_row: 5, row_above_blank: true, occupied_cols: 2, chips }
    }

    const AREA: [f32; 4] = [0.0, 0.0, 800.0, 400.0];

    #[test]
    fn a_blank_row_above_gets_the_chips_left_aligned() {
        let v = view(vec![chip(ChipKind::Git, "main"), chip(ChipKind::Venv, "venv ml")]);
        let out =
            layout_prompt_chips(&v, AREA, 8.0, 20.0, 1.0, &colors(), None, &mut measure);
        assert_eq!(out.rects.len(), 2, "two chips draw two pills");
        assert_eq!(out.texts.len(), 2);
        assert!(
            (out.rects[0].rect[0] - AREA[0]).abs() < f32::EPSILON,
            "the Warp shape starts at the left edge"
        );
        let row_y = 4.0 * 20.0;
        assert!(
            out.rects[0].rect[1] >= row_y && out.rects[0].rect[1] < row_y + 20.0,
            "the chips sit on the row above the prompt"
        );
    }

    #[test]
    fn the_drawn_rect_is_the_hit_rect() {
        // The chrome discipline: what is drawn and what is clickable come
        // from one computation. Clicking the centre of the second chip must
        // return the second chip.
        let v = view(vec![chip(ChipKind::Git, "main"), chip(ChipKind::Cwd, "~/dev")]);
        let out =
            layout_prompt_chips(&v, AREA, 8.0, 20.0, 1.0, &colors(), None, &mut measure);
        let r = out.rects[1].rect;
        assert_eq!(
            out.hit.hit(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0),
            Some(HitRegion::PromptChip(ChipKind::Cwd)),
        );
    }

    #[test]
    fn on_the_prompt_row_the_chips_yield_to_typing() {
        // No blank row above: chips right-align, and hide entirely once the
        // typed text plus the margin would reach them.
        let mut v = view(vec![chip(ChipKind::Git, "main")]);
        v.row_above_blank = false;
        v.occupied_cols = 10;
        let roomy =
            layout_prompt_chips(&v, AREA, 8.0, 20.0, 1.0, &colors(), None, &mut measure);
        assert_eq!(roomy.rects.len(), 1, "with room, the chip shows on the prompt row");
        let right_edge = roomy.rects[0].rect[0] + roomy.rects[0].rect[2];
        assert!(
            (right_edge - (AREA[0] + AREA[2])).abs() < 0.5,
            "the on-row fallback hugs the right edge"
        );

        // A command typed nearly across the row: the chip must vanish, not
        // shrink and not overlap.
        v.occupied_cols = 98;
        let crowded =
            layout_prompt_chips(&v, AREA, 8.0, 20.0, 1.0, &colors(), None, &mut measure);
        assert!(crowded.rects.is_empty(), "chips never sit over typed text");
        assert!(crowded.hit.hit(790.0, 110.0).is_none(), "a hidden chip is not clickable");
    }

    #[test]
    fn an_empty_view_and_an_offscreen_prompt_draw_nothing() {
        let none = layout_prompt_chips(
            &view(Vec::new()),
            AREA,
            8.0,
            20.0,
            1.0,
            &colors(),
            None,
            &mut measure,
        );
        assert!(none.rects.is_empty());

        let mut v = view(vec![chip(ChipKind::Git, "main")]);
        v.prompt_row = 999;
        let off = layout_prompt_chips(&v, AREA, 8.0, 20.0, 1.0, &colors(), None, &mut measure);
        assert!(off.rects.is_empty(), "a prompt outside the viewport anchors nothing");
    }
}
