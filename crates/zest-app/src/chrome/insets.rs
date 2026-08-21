//! What the chrome takes from each window edge.
//!
//! The grid does not own the window; it gets the rectangle left over after
//! the padding and (soon) the tab strip have taken their share. Keeping that
//! subtraction in one type is what lets the strip move from the top edge to
//! the left without touching every place that converts between pixels and
//! cells — resize, redraw, pointer hit testing and IME placement all ask the
//! same `Insets` the same question.

use zest_config::settings::TabsPosition;
use zest_font::CellMetrics;

/// Physical pixels taken from each window edge before the grid begins.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    pub top: f32,
    pub left: f32,
    pub right: f32,
    pub bottom: f32,
}

/// What a visible tab strip claims beyond the bare padding: which edge it
/// occupies, and the logical sizes the settings give it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripClaim {
    pub position: TabsPosition,
    /// `tabs.strip_height`, logical px.
    pub strip_height: u32,
    /// `tabs.sidebar_width`, logical px.
    pub sidebar_width: u32,
}

impl Insets {
    /// Bare window padding on all four sides, no chrome.
    ///
    /// `padding` is in logical pixels — it is the user-facing
    /// `window.padding` setting — so it scales with the display. The old
    /// hard-coded constant was applied unscaled, which quietly halved the
    /// intended padding on every HiDPI display.
    #[must_use]
    pub fn padding_only(padding: u32, scale: f32) -> Self {
        let p = padding as f32 * scale.max(0.1);
        Self { top: p, left: p, right: p, bottom: p }
    }

    /// The chrome's whole claim on the window edges: padding everywhere,
    /// plus the strip's edge when one is shown.
    ///
    /// Every edge comes out a whole physical pixel, because the top/left
    /// insets become the grid's origin and the atlas sampler is `Nearest`
    /// on a 1:1 assumption — at 125% (the ordinary Windows laptop scale)
    /// the default strip resolves to 8·1.25 + 46·1.25 = 67.5, and a
    /// half-pixel origin resamples every glyph between texels, which reads
    /// as a blurry font. Floored rather than rounded up so the chrome
    /// never grows: rounding an inset up can cost the grid its last row
    /// or column in a window sized exactly around it. (#85)
    #[must_use]
    pub fn resolved(padding: u32, scale: f32, strip: Option<StripClaim>) -> Self {
        let mut insets = Self::padding_only(padding, scale);
        if let Some(claim) = strip {
            match claim.position {
                TabsPosition::Top => {
                    insets.top += claim.strip_height as f32 * scale;
                }
                TabsPosition::Left => {
                    insets.left += claim.sidebar_width as f32 * scale;
                    // The full-width header over the sidebar + pane row: the
                    // vertical layout's counterpart of the strip, and it was
                    // once forgotten here — the grid painted its first two
                    // rows straight over the session name.
                    insets.top += super::layout::HEADER_H * scale;
                }
            }
            // Nothing reserves the bottom edge: the status bar is gone
            // (design §1), so below the grid there is only `window.padding`.
        }
        Self {
            top: insets.top.floor(),
            left: insets.left.floor(),
            right: insets.right.floor(),
            bottom: insets.bottom.floor(),
        }
    }

    /// The grid's rectangle inside a window of `w` × `h` physical pixels,
    /// as `[x, y, width, height]` — the shape a `Viewport` wants.
    #[must_use]
    pub fn grid_rect(&self, w: u32, h: u32) -> [f32; 4] {
        let width = (w as f32 - self.left - self.right).max(0.0);
        let height = (h as f32 - self.top - self.bottom).max(0.0);
        [self.left, self.top, width, height]
    }

    /// Columns and rows that fit in the grid's rectangle.
    ///
    /// Mirrors `CellMetrics::grid_size`'s floor-and-clamp semantics — at
    /// least one cell each way, however small the window gets — but takes
    /// per-edge insets, which symmetric padding cannot express once a strip
    /// occupies one edge only.
    #[must_use]
    pub fn grid_dims(&self, m: CellMetrics, w: u32, h: u32) -> (u16, u16) {
        let [_, _, gw, gh] = self.grid_rect(w, h);
        let cols = ((gw as u32) / m.cell_w.max(1)).max(1).min(u32::from(u16::MAX)) as u16;
        let rows = ((gh as u32) / m.cell_h.max(1)).max(1).min(u32::from(u16::MAX)) as u16;
        (cols, rows)
    }

    /// The window that holds exactly `cols` × `rows` cells, in physical pixels.
    ///
    /// The inverse of [`Insets::grid_dims`], and kept beside it deliberately:
    /// the pair round-trips, and that round trip is the whole correctness
    /// argument for `window.columns` / `window.rows`. A window sized from cells
    /// has to give the chrome its share back, or asking for 100×30 produces a
    /// grid of 96×27 and the setting looks approximate rather than exact.
    ///
    /// At least one cell each way, mirroring `grid_dims`' clamp — a config with
    /// `columns = 0` must not produce a zero-width window, which some platforms
    /// refuse outright and others render as a title bar with nothing under it.
    #[must_use]
    pub fn window_size(&self, m: CellMetrics, cols: u16, rows: u16) -> (u32, u32) {
        let cells_w = u32::from(cols.max(1)) * m.cell_w.max(1);
        let cells_h = u32::from(rows.max(1)) * m.cell_h.max(1);
        // Rounded up: the insets are `f32` (a scaled logical padding, so
        // routinely fractional) while a window size is whole pixels. Truncating
        // would leave the grid a pixel short of its own cell count and cost the
        // last column, which is the failure this rounding exists to prevent.
        let w = cells_w + (self.left + self.right).ceil().max(0.0) as u32;
        let h = cells_h + (self.top + self.bottom).ceil().max(0.0) as u32;
        (w, h)
    }
}

/// Where a `cols` × `rows` grid sits inside `area`: centered when it is
/// smaller than the pane, exactly `area` otherwise.
///
/// Under size arbitration (#215) a session is the *smallest* attached
/// client's size, so this window's grid can be narrower or shorter than the
/// pane it is drawn in. Centering happens per axis and only on a strict
/// shortfall in whole cells; a grid that fills the pane's cell capacity gets
/// the pane's exact rectangle, sub-cell remainder and all, so a local session
/// renders byte-identically to what it did before arbitration existed.
///
/// A grid *larger* than the pane (the moment between a foreign grow and this
/// window's next resize) keeps the pane's rectangle and lets the renderer
/// clip, which is what it did before too.
///
/// Everything that converts between pixels and cells must use the same
/// rectangle — the renderer's viewport, pointer hit testing, IME placement
/// and the block headers — or a click lands one letterbox-offset away from
/// the glyph it visibly touched.
#[must_use]
pub fn letterbox(area: [f32; 4], cols: usize, rows: usize, m: CellMetrics) -> [f32; 4] {
    let cw = m.cell_w.max(1) as f32;
    let ch = m.cell_h.max(1) as f32;
    let fit_cols = ((area[2] / cw) as usize).max(1);
    let fit_rows = ((area[3] / ch) as usize).max(1);
    let [mut x, mut y, mut w, mut h] = area;
    if cols < fit_cols {
        w = cols as f32 * cw;
        // The whole coordinate is floored, not just the centering term: the
        // pane's own origin can be fractional (padding times a fractional
        // scale), and a half-pixel origin samples every glyph between texels
        // -- it reads as a blurry font, not as an offset.
        x = (area[0] + (area[2] - w) / 2.0).floor();
    }
    if rows < fit_rows {
        h = rows as f32 * ch;
        y = (area[1] + (area[3] - h) / 2.0).floor();
    }
    [x, y, w, h]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cell_w: u32, cell_h: u32) -> CellMetrics {
        CellMetrics {
            cell_w,
            cell_h,
            baseline: cell_h.saturating_sub(4),
            underline_y: cell_h.saturating_sub(2),
            underline_thickness: 1,
            strikeout_y: cell_h / 2,
        }
    }

    #[test]
    fn a_window_sized_from_cells_holds_exactly_those_cells() {
        // The round trip is the correctness argument for window.columns/rows:
        // sizing a window from a cell count is only meaningful if asking it
        // back gives the same count. Across cell sizes, insets and asymmetric
        // chrome -- a tab strip takes from one edge only, which is where a
        // symmetric-padding assumption would break.
        let cases = [
            (metrics(10, 20), Insets::padding_only(0, 1.0)),
            (metrics(10, 20), Insets::padding_only(8, 1.0)),
            (metrics(10, 20), Insets::padding_only(8, 2.0)),
            (metrics(7, 15), Insets { top: 46.0, left: 8.0, right: 8.0, bottom: 28.0 }),
            (metrics(9, 19), Insets { top: 0.0, left: 262.0, right: 0.0, bottom: 0.0 }),
            // Fractional insets are the ordinary case, not an edge one: the
            // padding is logical pixels times a scale factor.
            (metrics(11, 23), Insets { top: 23.5, left: 8.5, right: 8.5, bottom: 13.5 }),
        ];
        for (m, i) in cases {
            for (cols, rows) in [(1, 1), (30, 10), (100, 30), (240, 67)] {
                let (w, h) = i.window_size(m, cols, rows);
                assert_eq!(
                    i.grid_dims(m, w, h),
                    (cols, rows),
                    "{cols}x{rows} cells at cell {}x{} with insets {i:?} must round-trip",
                    m.cell_w,
                    m.cell_h
                );
            }
        }
    }

    #[test]
    fn a_zero_cell_count_still_makes_a_window() {
        // `columns = 0` is a hand-edited config away, and a zero-size window is
        // refused outright by some platforms and drawn as empty chrome by
        // others. Clamped to one cell, mirroring grid_dims' own floor.
        let m = metrics(10, 20);
        let i = Insets::padding_only(8, 1.0);
        let (w, h) = i.window_size(m, 0, 0);
        assert_eq!(i.grid_dims(m, w, h), (1, 1), "one cell, never none");
        assert!(w > 0 && h > 0);
    }

    #[test]
    fn symmetric_insets_agree_with_the_font_crate() {
        // The old path was CellMetrics::grid_size(w, h, padding). Anything
        // this type computes for plain padding must match it, or the pty is
        // told a different size than the renderer draws.
        let m = metrics(10, 20);
        let i = Insets::padding_only(10, 1.0);
        assert_eq!(i.grid_dims(m, 1000, 400), m.grid_size(1000, 400, 10));
        assert_eq!(i.grid_dims(m, 1, 1), m.grid_size(1, 1, 10), "degenerate windows still agree");
    }

    #[test]
    fn resolved_insets_are_whole_physical_pixels_at_every_scale() {
        // 125/150/175% are the ordinary Windows laptop scales, and the
        // default strip (46) and sidebar (262) both go fractional there —
        // 46·1.25 = 57.5. The top/left insets are the grid's origin, and a
        // fractional origin resamples every glyph between texels through the
        // Nearest atlas sampler: stems snap inconsistently, edges go ragged.
        // (#85)
        let strips = [
            None,
            // The defaults, and a few awkward custom sizes — odd numbers stay
            // fractional at every non-integral scale in the table.
            Some(StripClaim { position: TabsPosition::Top, strip_height: 46, sidebar_width: 262 }),
            Some(StripClaim { position: TabsPosition::Left, strip_height: 46, sidebar_width: 262 }),
            Some(StripClaim { position: TabsPosition::Top, strip_height: 33, sidebar_width: 199 }),
            Some(StripClaim { position: TabsPosition::Left, strip_height: 33, sidebar_width: 199 }),
        ];
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for padding in [0u32, 5, 8, 13] {
                for strip in strips {
                    let i = Insets::resolved(padding, scale, strip);
                    for (edge, v) in
                        [("top", i.top), ("left", i.left), ("right", i.right), ("bottom", i.bottom)]
                    {
                        assert_eq!(
                            v,
                            v.floor(),
                            "{edge} inset {v} (scale {scale}, padding {padding}, strip \
                             {strip:?}) must be whole physical px — a fractional inset \
                             puts the grid on a half-pixel origin"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_fractional_inset_floors_so_the_chrome_never_grows() {
        // Floor, not round-half-up: an inset rounded upward enlarges the
        // chrome's claim, and in a window sized exactly around the grid that
        // is the last row or column gone. 8·1.25 + 46·1.25 = 67.5 → 67.
        let strip =
            StripClaim { position: TabsPosition::Top, strip_height: 46, sidebar_width: 262 };
        let i = Insets::resolved(8, 1.25, Some(strip));
        assert_eq!(i.top, 67.0, "the half pixel goes to the grid, not the chrome");
        assert_eq!(i.left, 10.0, "an already-whole product is untouched");

        // The sidebar layout claims two edges, and both must snap: the left
        // one for the sidebar, the top one for the full-width header.
        let sidebar =
            StripClaim { position: TabsPosition::Left, strip_height: 46, sidebar_width: 262 };
        let i = Insets::resolved(8, 1.25, Some(sidebar));
        assert_eq!(i.left, 337.0, "10 + 262·1.25 = 337.5 floors to 337");
        assert_eq!(i.top, 67.0, "10 + HEADER_H·1.25 = 67.5 floors to 67");
    }

    #[test]
    fn padding_scales_with_the_display() {
        // 8 logical px on a 2x display is 16 physical. The unscaled constant
        // this replaces drew visibly tighter margins on every HiDPI screen.
        let i = Insets::padding_only(8, 2.0);
        assert_eq!(i.left, 16.0);
        assert_eq!(i.grid_rect(100, 100), [16.0, 16.0, 68.0, 68.0]);
    }

    #[test]
    fn asymmetric_insets_place_the_grid_off_centre() {
        // The whole reason this type exists: a top strip moves only the top
        // edge, a sidebar only the left one.
        let m = metrics(10, 20);
        let i = Insets { top: 38.0, left: 8.0, right: 8.0, bottom: 8.0 };
        assert_eq!(i.grid_rect(1000, 400), [8.0, 38.0, 984.0, 354.0]);
        assert_eq!(i.grid_dims(m, 1000, 400), (98, 17));
    }

    #[test]
    fn a_smaller_grid_is_centered_in_the_pane() {
        // The arbitration case (#215): another, smaller client owns the size,
        // and this window letterboxes -- per axis, in whole pixels.
        let m = metrics(10, 20);
        let area = [8.0, 38.0, 1000.0, 400.0]; // fits 100 x 20
        assert_eq!(
            letterbox(area, 60, 10, m),
            [8.0 + 200.0, 38.0 + 100.0, 600.0, 200.0],
            "a smaller grid must sit centered, not pinned top-left"
        );
        assert_eq!(
            letterbox(area, 60, 20, m),
            [208.0, 38.0, 600.0, 400.0],
            "the shortfall is per axis: full-height grids center only horizontally"
        );

        // A pane origin of padding x a fractional scale: the letterboxed
        // origin must still land on a whole pixel, which means flooring the
        // full coordinate rather than the centering term alone.
        let fractional = [7.5, 38.5, 1000.0, 400.0];
        assert_eq!(
            letterbox(fractional, 60, 10, m),
            [207.0, 138.0, 600.0, 200.0],
            "a fractional pane origin must not leak a half-pixel into the letterbox"
        );
    }

    #[test]
    fn a_grid_that_fills_the_pane_keeps_the_exact_pane_rect() {
        // The local-session case must be byte-identical to life before
        // arbitration: same origin, same width, sub-cell remainder and all --
        // this is what keeps every existing pixel assertion (#44) true.
        let m = metrics(10, 20);
        let area = [8.0, 38.0, 1005.0, 407.0]; // fits 100 x 20, with remainder
        assert_eq!(letterbox(area, 100, 20, m), area);
        // Transiently larger (a foreign grow this window has not resized to
        // yet): still the pane's rect, and the renderer clips as it always did.
        assert_eq!(letterbox(area, 140, 50, m), area);
    }

    #[test]
    fn a_window_smaller_than_its_insets_still_yields_one_cell() {
        // A grid of zero cells panics deep in the terminal; clamping here is
        // what the font crate does and what the pty contract expects.
        let m = metrics(10, 20);
        let i = Insets { top: 200.0, left: 200.0, right: 200.0, bottom: 200.0 };
        assert_eq!(i.grid_dims(m, 100, 100), (1, 1));
        let [x, y, w, h] = i.grid_rect(100, 100);
        assert_eq!((w, h), (0.0, 0.0), "the rect clamps at zero rather than going negative");
        assert_eq!((x, y), (200.0, 200.0));
    }
}
