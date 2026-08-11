//! Box drawing and block elements, drawn into the cell rather than borrowed
//! from a font.
//!
//! # Why this exists
//!
//! A font's `─` is a glyph like any other: it is as wide as the font's advance,
//! which is *not* the cell. `cell_w` is that advance rounded to a whole pixel
//! ([`CellMetrics::derive`]), so every cell carries a fraction of a pixel of
//! slack that a horizontal rule cannot bridge — and `line_height` adds vertical
//! slack on top, split above and below the baseline, which breaks stacked `│`
//! and `█` the same way. The result is a rule with gaps in it and a full-block
//! run that renders as a picket fence. It is not a small-size artefact; it is
//! just as visible at 20px.
//!
//! No amount of font choice fixes this, because the mismatch is between the
//! font's advance and the *rounded* cell. So these ranges are generated at
//! exactly `cell_w × cell_h` instead, which makes tiling exact by construction:
//! a horizontal arm spans column 0 to `cell_w`, so the next cell continues it
//! with no seam at all.
//!
//! Kitty, Ghostty, WezTerm and Alacritty all do this, for this reason.
//!
//! # What is drawn
//!
//! U+2500–U+257F (box drawing) and U+2580–U+259F (block elements). Arrows are
//! deliberately left to the font: they are glyphs that happen to live nearby,
//! they carry no tiling requirement, and drawing them here would mean inventing
//! an arrowhead the font already has an opinion about.
//!
//! # The one deliberate difference from most fonts
//!
//! The shades (U+2591–U+2593) are flat coverage, not a dither pattern. A dither
//! has to be indexed by position to tile, and the only position available here
//! is cell-local — so a 4×4 pattern seams at every cell boundary unless the cell
//! happens to be a multiple of 4 pixels, which it is not. Seamless is the entire
//! point of this module, so the texture goes.

use crate::metrics::CellMetrics;
use crate::GlyphImage;

/// How heavy one arm of a line character is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

impl Weight {
    /// Whether this arm is drawn as two parallel rails.
    const fn is_double(self) -> bool {
        matches!(self, Self::Double)
    }
}

/// The four arms of a line character, in the order up, right, down, left.
///
/// Every non-dashed, non-curved character in U+2500–U+256C is exactly this plus
/// a rule for how the arms meet, which is why the table below is short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arms {
    up: Weight,
    right: Weight,
    down: Weight,
    left: Weight,
}

impl Arms {
    const NONE: Self =
        Self { up: Weight::None, right: Weight::None, down: Weight::None, left: Weight::None };

    const fn new(up: Weight, right: Weight, down: Weight, left: Weight) -> Self {
        Self { up, right, down, left }
    }

    const fn any_double(self) -> bool {
        self.up.is_double()
            || self.right.is_double()
            || self.down.is_double()
            || self.left.is_double()
    }
}

/// Pixel geometry for one cell, resolved once per rasterization.
struct Geom {
    w: u32,
    h: u32,
    /// Thickness of a light stroke, in whole pixels.
    light: u32,
    /// Thickness of a heavy stroke.
    heavy: u32,
}

impl Geom {
    fn new(cell: CellMetrics) -> Self {
        // Derived from the font's own stroke size (which is what
        // `underline_thickness` already is) so a rule looks like it belongs to
        // the text rather than to the renderer. Clamped so an eccentric font
        // cannot make a "light" line thicker than the cell it lives in.
        let cap = (cell.cell_h / 3).max(1);
        let light = cell.underline_thickness.clamp(1, cap);
        // Heavy must be visibly heavier at every size, including the 1px case
        // where doubling is the only thing that separates them.
        let heavy = (light * 2).clamp(light + 1, (cell.cell_h / 2).max(light + 1));
        Self { w: cell.cell_w, h: cell.cell_h, light, heavy }
    }

    const fn thickness(&self, w: Weight) -> u32 {
        match w {
            Weight::None => 0,
            Weight::Light | Weight::Double => self.light,
            Weight::Heavy => self.heavy,
        }
    }

    /// Total width of a doubled stroke: two rails with a gap of one rail.
    const fn double_span(&self) -> u32 {
        self.light * 3
    }

    /// Start of a band of `t` pixels centred in a span of `total`.
    ///
    /// Integer arithmetic on purpose: a band that lands on a half pixel is the
    /// bug this module exists to remove.
    const fn centre(total: u32, t: u32) -> u32 {
        if total > t { (total - t) / 2 } else { 0 }
    }
}

/// A coverage buffer for one cell.
struct Mask {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

impl Mask {
    fn new(w: u32, h: u32) -> Self {
        Self { w, h, data: vec![0; (w * h) as usize] }
    }

    /// Fill a half-open rectangle, clamped to the cell.
    fn rect(&mut self, x0: u32, x1: u32, y0: u32, y1: u32) {
        self.rect_alpha(x0, x1, y0, y1, 255);
    }

    fn rect_alpha(&mut self, x0: u32, x1: u32, y0: u32, y1: u32, a: u8) {
        let x0 = x0.min(self.w);
        let x1 = x1.min(self.w);
        let y0 = y0.min(self.h);
        let y1 = y1.min(self.h);
        for y in y0..y1 {
            let row = (y * self.w) as usize;
            for x in x0..x1 {
                let p = &mut self.data[row + x as usize];
                *p = (*p).max(a);
            }
        }
    }

    /// Accumulate coverage from a supersampled shape.
    ///
    /// Curves and diagonals are the only things here that are not axis-aligned,
    /// and they are the only things that need antialiasing — the straight arms
    /// are snapped to whole pixels precisely so they do not get any.
    fn shade<F: Fn(f32, f32) -> bool>(&mut self, inside: F) {
        const S: u32 = 4;
        let step = 1.0 / f32::from(S as u16);
        for y in 0..self.h {
            for x in 0..self.w {
                let mut hits = 0u32;
                for sy in 0..S {
                    for sx in 0..S {
                        let px = x as f32 + (sx as f32 + 0.5) * step;
                        let py = y as f32 + (sy as f32 + 0.5) * step;
                        if inside(px, py) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    let a = ((hits * 255) / (S * S)) as u8;
                    let p = &mut self.data[(y * self.w + x) as usize];
                    *p = (*p).max(a);
                }
            }
        }
    }

    fn into_image(self, cell: CellMetrics) -> GlyphImage {
        GlyphImage {
            width: self.w,
            height: self.h,
            // The quad is placed at `pen_x + left, baseline - top`, and pen_x is
            // the cell's left edge, so these two put the mask exactly on the
            // cell — which is the whole contract of this module.
            left: 0,
            top: cell.baseline as i32,
            data: self.data,
            is_color: false,
        }
    }
}

/// Whether this codepoint is drawn here rather than taken from a font.
#[must_use]
pub fn covers(ch: char) -> bool {
    matches!(ch as u32, 0x2500..=0x259F)
}

/// Draw one box-drawing or block-element character at exactly cell size.
///
/// Returns `None` for codepoints outside the covered ranges, and for the few
/// inside them that have no agreed rendering, so the caller can fall back to
/// the font.
#[must_use]
pub fn render(ch: char, cell: CellMetrics) -> Option<GlyphImage> {
    if !covers(ch) || cell.cell_w == 0 || cell.cell_h == 0 {
        return None;
    }
    let g = Geom::new(cell);
    let mut m = Mask::new(g.w, g.h);
    let cp = ch as u32;

    let drawn = match cp {
        0x2500..=0x257F => draw_line(&mut m, &g, cp),
        0x2580..=0x259F => draw_block(&mut m, &g, cp),
        _ => false,
    };

    drawn.then(|| m.into_image(cell))
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

fn draw_line(m: &mut Mask, g: &Geom, cp: u32) -> bool {
    // Dashes first: they are the one family whose *gaps are the point*, so they
    // must not go through the solid-arm path that guarantees tiling.
    if let Some((count, weight, horizontal)) = dashed(cp) {
        draw_dashed(m, g, count, weight, horizontal);
        return true;
    }
    // Arcs and diagonals are the only curved/oblique shapes here.
    if (0x256D..=0x2570).contains(&cp) {
        draw_arc(m, g, cp);
        return true;
    }
    if (0x2571..=0x2573).contains(&cp) {
        draw_diagonal(m, g, cp);
        return true;
    }

    let arms = arms_of(cp);
    if arms == Arms::NONE {
        return false;
    }
    if arms.any_double() {
        draw_double(m, g, arms);
    } else {
        draw_simple(m, g, arms);
    }
    true
}

/// Light and heavy arms, in any combination.
///
/// Each arm runs from its cell edge *through* the junction to the far side of
/// the perpendicular band, so a corner is solid without a special case for the
/// centre.
fn draw_simple(m: &mut Mask, g: &Geom, a: Arms) {
    let tv = g.thickness(a.up).max(g.thickness(a.down));
    let th = g.thickness(a.left).max(g.thickness(a.right));

    let cx0 = Geom::centre(g.w, tv);
    let cx1 = cx0 + tv;
    let cy0 = Geom::centre(g.h, th);
    let cy1 = cy0 + th;

    if a.left != Weight::None {
        let t = g.thickness(a.left);
        let y0 = Geom::centre(g.h, t);
        m.rect(0, cx1.max(Geom::centre(g.w, 0)), y0, y0 + t);
    }
    if a.right != Weight::None {
        let t = g.thickness(a.right);
        let y0 = Geom::centre(g.h, t);
        m.rect(cx0.min(g.w), g.w, y0, y0 + t);
    }
    if a.up != Weight::None {
        let t = g.thickness(a.up);
        let x0 = Geom::centre(g.w, t);
        m.rect(x0, x0 + t, 0, cy1.max(Geom::centre(g.h, 0)));
    }
    if a.down != Weight::None {
        let t = g.thickness(a.down);
        let x0 = Geom::centre(g.w, t);
        m.rect(x0, x0 + t, cy0.min(g.h), g.h);
    }
}

/// Anything with a doubled arm.
///
/// Doubles are four rails — two horizontal, two vertical — not four arms, which
/// is why this cannot reuse [`draw_simple`]: that knows only about a single
/// centred band, and a double junction has a hole in the middle of it.
///
/// The whole shape follows from one distinction. In a corner, one rail of each
/// pair is **outer** (it wraps the turn and meets the far rail of the other
/// pair) and one is **inner** (it stops at the near rail). In a tee or a cross
/// the arm passes straight through, so neither rail is outer and the rail the
/// arms arrive at is *broken* where they meet it — which is exactly why `╬` is
/// four corner pieces with an empty centre rather than a filled junction.
///
/// Getting this wrong is not subtle once a table is on screen: `╠` comes out
/// with its left rail chopped, or `╔` nests the wrong way round.
fn draw_double(m: &mut Mask, g: &Geom, a: Arms) {
    let t = g.light;
    let span = g.double_span();

    let vx = Geom::centre(g.w, span);
    let (vl0, vl1) = (vx, vx + t); // left rail
    let (vr0, vr1) = (vx + span - t, vx + span); // right rail
    let hy = Geom::centre(g.h, span);
    let (ht0, ht1) = (hy, hy + t); // top rail
    let (hb0, hb1) = (hy + span - t, hy + span); // bottom rail

    let (up, down) = (a.up.is_double(), a.down.is_double());
    let (left, right) = (a.left.is_double(), a.right.is_double());

    // A light arm crossing a double one keeps its ordinary centred band, and the
    // rails have to stop against *that* band rather than at the cell edge --
    // U+2552 ╒ is a double horizontal meeting a light vertical.
    let lv = g.thickness(a.up).max(g.thickness(a.down));
    let lv0 = Geom::centre(g.w, lv);
    let lh = g.thickness(a.left).max(g.thickness(a.right));
    let lh0 = Geom::centre(g.h, lh);

    // Which rail wraps the turn. `None` when the arm passes straight through,
    // because then neither does.
    let h_outer = (up != down).then_some(down); // Some(true) => the top rail
    let v_outer = (left != right).then_some(right); // Some(true) => the left rail

    if left || right {
        for (y0, y1, is_top) in [(ht0, ht1, true), (hb0, hb1, false)] {
            let outer = h_outer == Some(is_top);
            let vertical = up || down;
            let x0 = if left {
                0
            } else if vertical {
                if outer { vl0 } else { vr0 }
            } else {
                lv0.min(g.w)
            };
            let x1 = if right {
                g.w
            } else if vertical {
                if outer { vr1 } else { vl1 }
            } else {
                (lv0 + lv).max(x0)
            };

            if left && right && vertical && !outer {
                // Passes through: the centre belongs to the hole.
                m.rect(0, vl1, y0, y1);
                m.rect(vr0, g.w, y0, y1);
            } else {
                m.rect(x0, x1, y0, y1);
            }
        }
    }

    if up || down {
        for (x0, x1, is_left) in [(vl0, vl1, true), (vr0, vr1, false)] {
            let outer = v_outer == Some(is_left);
            let horizontal = left || right;
            let y0 = if up {
                0
            } else if horizontal {
                if outer { ht0 } else { hb0 }
            } else {
                lh0.min(g.h)
            };
            let y1 = if down {
                g.h
            } else if horizontal {
                if outer { hb1 } else { ht1 }
            } else {
                (lh0 + lh).max(y0)
            };

            if up && down && horizontal && !outer {
                m.rect(x0, x1, 0, ht1);
                m.rect(x0, x1, hb0, g.h);
            } else {
                m.rect(x0, x1, y0, y1);
            }
        }
    }

    // Now the light arms, if any: U+256A ╪ and U+256B ╫ and the mixed corners.
    let single = Arms::new(
        if up { Weight::None } else { a.up },
        if right { Weight::None } else { a.right },
        if down { Weight::None } else { a.down },
        if left { Weight::None } else { a.left },
    );
    if single != Arms::NONE {
        draw_simple(m, g, single);
    }
}

/// Dashed horizontals and verticals.
///
/// Returns the dash count, the stroke weight, and whether it runs horizontally.
const fn dashed(cp: u32) -> Option<(u32, Weight, bool)> {
    let (count, weight, horizontal) = match cp {
        0x2504 => (3, Weight::Light, true),
        0x2505 => (3, Weight::Heavy, true),
        0x2506 => (3, Weight::Light, false),
        0x2507 => (3, Weight::Heavy, false),
        0x2508 => (4, Weight::Light, true),
        0x2509 => (4, Weight::Heavy, true),
        0x250A => (4, Weight::Light, false),
        0x250B => (4, Weight::Heavy, false),
        0x254C => (2, Weight::Light, true),
        0x254D => (2, Weight::Heavy, true),
        0x254E => (2, Weight::Light, false),
        0x254F => (2, Weight::Heavy, false),
        _ => return None,
    };
    Some((count, weight, horizontal))
}

fn draw_dashed(m: &mut Mask, g: &Geom, count: u32, weight: Weight, horizontal: bool) {
    let t = g.thickness(weight);
    let total = if horizontal { g.w } else { g.h };
    // Gap is a quarter of each segment's share, so the dashes read as a broken
    // line at any cell size rather than as dots at small ones.
    let share = total as f32 / count as f32;
    let gap = (share * 0.25).round().max(1.0) as u32;

    for i in 0..count {
        let start = (i as f32 * share).round() as u32;
        let end = (((i + 1) as f32) * share).round() as u32;
        let end = end.saturating_sub(gap).max(start);
        if horizontal {
            let y0 = Geom::centre(g.h, t);
            m.rect(start, end, y0, y0 + t);
        } else {
            let x0 = Geom::centre(g.w, t);
            m.rect(x0, x0 + t, start, end);
        }
    }
}

/// The rounded corners, U+256D–U+2570.
fn draw_arc(m: &mut Mask, g: &Geom, cp: u32) {
    let t = g.light as f32;
    let w = g.w as f32;
    let h = g.h as f32;
    let cx = (Geom::centre(g.w, g.light) as f32) + t / 2.0;
    let cy = (Geom::centre(g.h, g.light) as f32) + t / 2.0;

    // Which way the arc turns: (goes right, goes down).
    let (to_right, to_down) = match cp {
        0x256D => (true, true),   // ╭ down and right
        0x256E => (false, true),  // ╮ down and left
        0x256F => (false, false), // ╯ up and left
        _ => (true, false),       // ╰ up and right
    };

    // The arc's centre is the corner of the square it rounds; radius reaches
    // from there to the two straight arms it joins.
    let r = (cx.min(w - cx)).min(cy.min(h - cy)).max(t);
    let ox = if to_right { cx + r } else { cx - r };
    let oy = if to_down { cy + r } else { cy - r };

    m.shade(|x, y| {
        // Only the quadrant between the two arms.
        let in_quadrant =
            if to_right { x >= cx } else { x <= cx } && if to_down { y >= cy } else { y <= cy };
        if !in_quadrant {
            return false;
        }
        let d = ((x - ox).powi(2) + (y - oy).powi(2)).sqrt();
        (d - r).abs() <= t / 2.0
    });

    // The straight tails from the arc to the cell edges.
    let vx = Geom::centre(g.w, g.light);
    let hy = Geom::centre(g.h, g.light);
    if to_down {
        m.rect(vx, vx + g.light, (cy + r) as u32, g.h);
    } else {
        m.rect(vx, vx + g.light, 0, (cy - r) as u32);
    }
    if to_right {
        m.rect((cx + r) as u32, g.w, hy, hy + g.light);
    } else {
        m.rect(0, (cx - r) as u32, hy, hy + g.light);
    }
}

/// The diagonals, U+2571–U+2573.
fn draw_diagonal(m: &mut Mask, g: &Geom, cp: u32) {
    let w = g.w as f32;
    let h = g.h as f32;
    let t = g.light as f32;
    // Perpendicular half-width of the stroke, so a diagonal looks the same
    // weight as a horizontal one rather than thinner.
    let half = t / 2.0 * ((w * w + h * h).sqrt() / h.max(1.0)).min(2.0);

    let forward = cp == 0x2571 || cp == 0x2573; // ╱ and ╳
    let back = cp == 0x2572 || cp == 0x2573; // ╲ and ╳

    m.shade(|x, y| {
        // Distance from the point to each diagonal, measured along x.
        if forward {
            // Bottom-left to top-right.
            let want = w * (1.0 - y / h);
            if (x - want).abs() * (h / (w * w + h * h).sqrt()) <= half {
                return true;
            }
        }
        if back {
            let want = w * (y / h);
            if (x - want).abs() * (h / (w * w + h * h).sqrt()) <= half {
                return true;
            }
        }
        false
    });
}

/// The four arms of every non-dashed, non-curved character in U+2500–U+256C.
#[allow(clippy::too_many_lines)]
fn arms_of(cp: u32) -> Arms {
    use Weight::{Double as D, Heavy as H, Light as L, None as N};
    match cp {
        0x2500 => Arms::new(N, L, N, L),
        0x2501 => Arms::new(N, H, N, H),
        0x2502 => Arms::new(L, N, L, N),
        0x2503 => Arms::new(H, N, H, N),

        0x250C => Arms::new(N, L, L, N),
        0x250D => Arms::new(N, H, L, N),
        0x250E => Arms::new(N, L, H, N),
        0x250F => Arms::new(N, H, H, N),
        0x2510 => Arms::new(N, N, L, L),
        0x2511 => Arms::new(N, N, L, H),
        0x2512 => Arms::new(N, N, H, L),
        0x2513 => Arms::new(N, N, H, H),
        0x2514 => Arms::new(L, L, N, N),
        0x2515 => Arms::new(L, H, N, N),
        0x2516 => Arms::new(H, L, N, N),
        0x2517 => Arms::new(H, H, N, N),
        0x2518 => Arms::new(L, N, N, L),
        0x2519 => Arms::new(L, N, N, H),
        0x251A => Arms::new(H, N, N, L),
        0x251B => Arms::new(H, N, N, H),

        0x251C => Arms::new(L, L, L, N),
        0x251D => Arms::new(L, H, L, N),
        0x251E => Arms::new(H, L, L, N),
        0x251F => Arms::new(L, L, H, N),
        0x2520 => Arms::new(H, L, H, N),
        0x2521 => Arms::new(H, H, L, N),
        0x2522 => Arms::new(L, H, H, N),
        0x2523 => Arms::new(H, H, H, N),
        0x2524 => Arms::new(L, N, L, L),
        0x2525 => Arms::new(L, N, L, H),
        0x2526 => Arms::new(H, N, L, L),
        0x2527 => Arms::new(L, N, H, L),
        0x2528 => Arms::new(H, N, H, L),
        0x2529 => Arms::new(H, N, L, H),
        0x252A => Arms::new(L, N, H, H),
        0x252B => Arms::new(H, N, H, H),

        0x252C => Arms::new(N, L, L, L),
        0x252D => Arms::new(N, L, L, H),
        0x252E => Arms::new(N, H, L, L),
        0x252F => Arms::new(N, H, L, H),
        0x2530 => Arms::new(N, L, H, L),
        0x2531 => Arms::new(N, L, H, H),
        0x2532 => Arms::new(N, H, H, L),
        0x2533 => Arms::new(N, H, H, H),
        0x2534 => Arms::new(L, L, N, L),
        0x2535 => Arms::new(L, L, N, H),
        0x2536 => Arms::new(L, H, N, L),
        0x2537 => Arms::new(L, H, N, H),
        0x2538 => Arms::new(H, L, N, L),
        0x2539 => Arms::new(H, L, N, H),
        0x253A => Arms::new(H, H, N, L),
        0x253B => Arms::new(H, H, N, H),

        0x253C => Arms::new(L, L, L, L),
        0x253D => Arms::new(L, L, L, H),
        0x253E => Arms::new(L, H, L, L),
        0x253F => Arms::new(L, H, L, H),
        0x2540 => Arms::new(H, L, L, L),
        0x2541 => Arms::new(L, L, H, L),
        0x2542 => Arms::new(H, L, H, L),
        0x2543 => Arms::new(H, L, L, H),
        0x2544 => Arms::new(H, H, L, L),
        0x2545 => Arms::new(L, L, H, H),
        0x2546 => Arms::new(L, H, H, L),
        0x2547 => Arms::new(H, H, L, H),
        0x2548 => Arms::new(L, H, H, H),
        0x2549 => Arms::new(H, L, H, H),
        0x254A => Arms::new(H, H, H, L),
        0x254B => Arms::new(H, H, H, H),

        0x2550 => Arms::new(N, D, N, D),
        0x2551 => Arms::new(D, N, D, N),
        0x2552 => Arms::new(N, D, L, N),
        0x2553 => Arms::new(N, L, D, N),
        0x2554 => Arms::new(N, D, D, N),
        0x2555 => Arms::new(N, N, L, D),
        0x2556 => Arms::new(N, N, D, L),
        0x2557 => Arms::new(N, N, D, D),
        0x2558 => Arms::new(L, D, N, N),
        0x2559 => Arms::new(D, L, N, N),
        0x255A => Arms::new(D, D, N, N),
        0x255B => Arms::new(L, N, N, D),
        0x255C => Arms::new(D, N, N, L),
        0x255D => Arms::new(D, N, N, D),
        0x255E => Arms::new(L, D, L, N),
        0x255F => Arms::new(D, L, D, N),
        0x2560 => Arms::new(D, D, D, N),
        0x2561 => Arms::new(L, N, L, D),
        0x2562 => Arms::new(D, N, D, L),
        0x2563 => Arms::new(D, N, D, D),
        0x2564 => Arms::new(N, D, L, D),
        0x2565 => Arms::new(N, L, D, L),
        0x2566 => Arms::new(N, D, D, D),
        0x2567 => Arms::new(L, D, N, D),
        0x2568 => Arms::new(D, L, N, L),
        0x2569 => Arms::new(D, D, N, D),
        0x256A => Arms::new(L, D, L, D),
        0x256B => Arms::new(D, L, D, L),
        0x256C => Arms::new(D, D, D, D),

        // U+2574–U+257B are half-length stubs, and U+257C–U+257F are the
        // light/heavy transitions. Both are ordinary arms.
        0x2574 => Arms::new(N, N, N, L),
        0x2575 => Arms::new(L, N, N, N),
        0x2576 => Arms::new(N, L, N, N),
        0x2577 => Arms::new(N, N, L, N),
        0x2578 => Arms::new(N, N, N, H),
        0x2579 => Arms::new(H, N, N, N),
        0x257A => Arms::new(N, H, N, N),
        0x257B => Arms::new(N, N, H, N),
        0x257C => Arms::new(N, H, N, L),
        0x257D => Arms::new(L, N, H, N),
        0x257E => Arms::new(N, L, N, H),
        0x257F => Arms::new(H, N, L, N),

        _ => Arms::NONE,
    }
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// Split a span into `n`ths the same way in every cell, so partial blocks in
/// adjacent cells line up.
fn frac(total: u32, num: u32, den: u32) -> u32 {
    ((u64::from(total) * u64::from(num) + u64::from(den) / 2) / u64::from(den)) as u32
}

fn draw_block(m: &mut Mask, g: &Geom, cp: u32) -> bool {
    let (w, h) = (g.w, g.h);
    match cp {
        // Upper half.
        0x2580 => m.rect(0, w, 0, frac(h, 1, 2)),
        // Lower one-eighth through seven-eighths.
        0x2581..=0x2587 => {
            let n = cp - 0x2580;
            m.rect(0, w, h - frac(h, n, 8), h);
        }
        // Full block. The reason this module exists.
        0x2588 => m.rect(0, w, 0, h),
        // Left seven-eighths (U+2589) down to left one-eighth (U+258F).
        0x2589..=0x258F => {
            let n = 0x2590 - cp;
            m.rect(0, frac(w, n, 8), 0, h);
        }
        // Right half.
        0x2590 => m.rect(frac(w, 1, 2), w, 0, h),
        // Shades: flat coverage, see the module header.
        0x2591 => m.rect_alpha(0, w, 0, h, 64),
        0x2592 => m.rect_alpha(0, w, 0, h, 128),
        0x2593 => m.rect_alpha(0, w, 0, h, 192),
        // Upper one-eighth.
        0x2594 => m.rect(0, w, 0, frac(h, 1, 8)),
        // Right one-eighth.
        0x2595 => m.rect(w - frac(w, 1, 8), w, 0, h),
        // Quadrants. Bit order: 1 = upper-left, 2 = upper-right,
        // 4 = lower-left, 8 = lower-right.
        0x2596..=0x259F => {
            let bits = match cp {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                _ => 0b1110,
            };
            let mx = frac(w, 1, 2);
            let my = frac(h, 1, 2);
            if bits & 0b0001 != 0 {
                m.rect(0, mx, 0, my);
            }
            if bits & 0b0010 != 0 {
                m.rect(mx, w, 0, my);
            }
            if bits & 0b0100 != 0 {
                m.rect(0, mx, my, h);
            }
            if bits & 0b1000 != 0 {
                m.rect(mx, w, my, h);
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell with awkward, realistic numbers: Cascadia Mono at 13px on a 1x
    /// display measures 8x20 with a 1px stroke, which is exactly where the bug
    /// this module fixes is most visible.
    fn cell() -> CellMetrics {
        CellMetrics {
            cell_w: 8,
            cell_h: 20,
            baseline: 16,
            underline_y: 18,
            underline_thickness: 1,
            strikeout_y: 10,
        }
    }

    fn mask_of(ch: char, cell: CellMetrics) -> GlyphImage {
        render(ch, cell).unwrap_or_else(|| panic!("{ch:?} should be drawn here"))
    }

    fn at(img: &GlyphImage, x: u32, y: u32) -> u8 {
        img.data[(y * img.width + x) as usize]
    }

    #[test]
    fn a_mask_is_exactly_the_cell() {
        // The entire point: the quad is cell-sized, so the next cell continues
        // it with no seam. A font glyph is the font's advance, which is not it.
        for ch in ['─', '│', '█', '╬', '╭', '▀', '▚'] {
            let img = mask_of(ch, cell());
            assert_eq!(
                (img.width, img.height),
                (cell().cell_w, cell().cell_h),
                "{ch:?} must fill the cell exactly"
            );
            assert_eq!(img.left, 0, "{ch:?} starts at the cell's left edge");
            assert_eq!(img.top, cell().baseline as i32, "{ch:?} sits on the cell, not the baseline");
        }
    }

    #[test]
    fn a_full_block_has_no_interior_gap() {
        // This is the regression test for the reported bug: a run of U+2588
        // rendered as a picket fence because the font's glyph was narrower than
        // the cell. Every pixel must be covered, or a run of them shows seams.
        let img = mask_of('█', cell());
        for y in 0..img.height {
            for x in 0..img.width {
                assert_eq!(at(&img, x, y), 255, "gap at {x},{y} in a full block");
            }
        }
    }

    #[test]
    fn a_horizontal_rule_spans_the_whole_width() {
        // A rule that stops short of either edge leaves a gap at every cell
        // boundary -- the same bug, one pixel at a time.
        let img = mask_of('─', cell());
        let row = (0..img.height)
            .find(|&y| at(&img, 0, y) > 0)
            .expect("the rule has to be somewhere");
        for x in 0..img.width {
            assert_eq!(at(&img, x, row), 255, "gap at column {x} of a horizontal rule");
        }
    }

    #[test]
    fn a_vertical_rule_spans_the_whole_height() {
        let img = mask_of('│', cell());
        let col = (0..img.width).find(|&x| at(&img, x, 0) > 0).expect("the rule has to be somewhere");
        for y in 0..img.height {
            assert_eq!(at(&img, col, y), 255, "gap at row {y} of a vertical rule");
        }
    }

    #[test]
    fn corners_meet_the_rules_they_join() {
        // A corner whose arm sits one pixel off the rule it continues is the
        // other half of the same bug, and is invisible until a table is drawn.
        let rule = mask_of('─', cell());
        let row = (0..rule.height).find(|&y| at(&rule, 0, y) > 0).unwrap();

        for ch in ['┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼'] {
            let img = mask_of(ch, cell());
            let has_left = matches!(ch, '┐' | '┘' | '┤' | '┬' | '┴' | '┼');
            let has_right = matches!(ch, '┌' | '└' | '├' | '┬' | '┴' | '┼');
            if has_left {
                assert_eq!(at(&img, 0, row), 255, "{ch:?} must meet a rule arriving from the left");
            }
            if has_right {
                assert_eq!(
                    at(&img, img.width - 1, row),
                    255,
                    "{ch:?} must meet a rule leaving to the right"
                );
            }
        }

        let vrule = mask_of('│', cell());
        let col = (0..vrule.width).find(|&x| at(&vrule, x, 0) > 0).unwrap();
        for ch in ['┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼'] {
            let img = mask_of(ch, cell());
            let has_up = matches!(ch, '└' | '┘' | '├' | '┤' | '┴' | '┼');
            let has_down = matches!(ch, '┌' | '┐' | '├' | '┤' | '┬' | '┼');
            if has_up {
                assert_eq!(at(&img, col, 0), 255, "{ch:?} must meet a rule arriving from above");
            }
            if has_down {
                assert_eq!(
                    at(&img, col, img.height - 1),
                    255,
                    "{ch:?} must meet a rule leaving below"
                );
            }
        }
    }

    #[test]
    fn stacked_verticals_join_across_rows() {
        // `line_height` puts slack above and below the baseline, which is what
        // broke stacked box borders when the glyph came from a font.
        let img = mask_of('│', cell());
        let col = (0..img.width).find(|&x| at(&img, x, 0) > 0).unwrap();
        assert_eq!(at(&img, col, 0), 255, "must reach the top of the cell");
        assert_eq!(at(&img, col, img.height - 1), 255, "and the bottom, or rows show a gap");
    }

    #[test]
    fn double_rules_span_the_width_too() {
        let img = mask_of('═', cell());
        let rows: Vec<u32> = (0..img.height).filter(|&y| at(&img, 0, y) > 0).collect();
        assert_eq!(rows.len(), 2, "a double rule is two rails");
        for row in rows {
            for x in 0..img.width {
                assert_eq!(at(&img, x, row), 255, "gap at column {x} of a double rule");
            }
        }
    }

    /// A cell wide enough that the two rails and their gap are separable.
    fn wide() -> CellMetrics {
        CellMetrics { cell_w: 16, cell_h: 20, ..cell() }
    }

    #[test]
    fn a_double_tee_keeps_its_through_rail_and_breaks_the_other() {
        // `╠` is `║` with `═` joining it from the right. The far rail runs the
        // whole height; the near one is interrupted where the horizontals meet
        // it. Get this backwards and a table's left edge is chopped into
        // fragments -- which is what makes double-line borders look broken
        // rather than merely wrong.
        let c = wide();
        let img = mask_of('╠', c);
        let cols: Vec<u32> =
            (0..img.width).filter(|&x| (0..img.height).any(|y| at(&img, x, y) > 0)).collect();
        let far = cols[0];
        assert!(
            (0..img.height).all(|y| at(&img, far, y) > 0),
            "the rail the arms do not touch must run the full height"
        );

        // The near rail is the other one inside the junction, and it must have a
        // gap where the horizontals leave.
        let near = *cols
            .iter()
            .find(|&&x| x > far && (0..img.height).any(|y| at(&img, x, y) == 0))
            .expect("the near rail must be interrupted");
        assert!(
            (0..img.height).any(|y| at(&img, near, y) == 0),
            "the rail the arms meet must be broken by them"
        );
    }

    /// Rows carrying ink in one column. Sampling a *column* is the point: the
    /// vertical rails ink almost every row, so scanning whole rows finds them
    /// too and says nothing about where the horizontal rails are.
    fn rail_rows(img: &GlyphImage, x: u32) -> Vec<u32> {
        (0..img.height).filter(|&y| at(img, x, y) > 0).collect()
    }

    fn rail_cols(img: &GlyphImage, y: u32) -> Vec<u32> {
        (0..img.width).filter(|&x| at(img, x, y) > 0).collect()
    }

    #[test]
    fn a_double_corner_nests_the_right_way_round() {
        // `╔`: the outer rails meet at the top-left of the junction and the
        // inner rails at the bottom-right. The tell is that the top rail
        // reaches further left than the bottom one.
        let c = wide();
        let img = mask_of('╔', c);
        // Sampled at the right edge, where only the horizontal rails reach.
        let rails = rail_rows(&img, c.cell_w - 1);
        let cols = rail_cols(&img, c.cell_h - 1);
        assert_eq!(rails.len(), 2, "a double corner has two horizontal rails");
        assert_eq!(cols.len(), 2, "and two vertical ones");

        // Sample the gap *between* the vertical rails. Measuring where a rail
        // starts does not work: the vertical rails ink those rows too, so both
        // horizontals appear to reach equally far left. In the gap only the
        // outer rail is present, which is precisely what nesting means.
        let gap_x = cols[0] + 1;
        assert!(gap_x < cols[1], "the rails must have a gap between them");
        assert!(at(&img, gap_x, rails[0]) > 0, "the outer rail crosses the gap to wrap the corner");
        assert_eq!(at(&img, gap_x, rails[1]), 0, "the inner rail stops at the near rail");

        // And the same one turn later, so this cannot pass by drawing two
        // horizontal rails and no corner at all.
        let gap_y = rails[0] + 1;
        assert!(gap_y < rails[1], "the horizontal rails must have a gap too");
        assert!(at(&img, cols[0], gap_y) > 0, "the outer vertical rail runs through the gap");
        assert_eq!(at(&img, cols[1], gap_y), 0, "the inner vertical rail starts below it");

        assert!(
            (0..rails[0]).all(|y| (0..img.width).all(|x| at(&img, x, y) == 0)),
            "a corner opening down and right draws nothing above itself"
        );
    }

    #[test]
    fn a_double_cross_has_a_hollow_centre() {
        // `╬` is four corner pieces, not a filled junction. A filled centre is
        // the giveaway that the rails were drawn as arms. The hole is derived
        // from the rails rather than assumed to be the middle pixel -- with a
        // 1px stroke it is one pixel across and not where `w/2, h/2` lands.
        let c = wide();
        let img = mask_of('╬', c);
        let rows = rail_rows(&img, 0); // horizontal rails, at the left edge
        let cols = rail_cols(&img, 0); // vertical rails, at the top edge
        assert_eq!(rows.len(), 2, "two horizontal rails");
        assert_eq!(cols.len(), 2, "two vertical rails");

        let mut hollow = 0;
        for y in rows[0] + 1..rows[1] {
            for x in cols[0] + 1..cols[1] {
                assert_eq!(at(&img, x, y), 0, "the centre of a double cross is a hole");
                hollow += 1;
            }
        }
        assert!(hollow > 0, "and the hole has to have some area, or nothing was checked");
    }

    #[test]
    fn a_double_rule_meets_the_corner_that_continues_it() {
        // The tiling rule again, for the double family: the rails of `═` and of
        // `╔` have to sit on the same rows, or a table's top edge steps where
        // the corner joins it.
        let c = wide();
        let rule = mask_of('═', c);
        let corner = mask_of('╔', c);
        let rows = rail_rows(&rule, 0);
        assert_eq!(rows, rail_rows(&corner, c.cell_w - 1), "rails must line up across cells");
        for y in rows {
            assert_eq!(
                at(&corner, c.cell_w - 1, y),
                255,
                "and the corner must reach the cell edge to meet the rule"
            );
        }
    }

    #[test]
    fn heavy_is_heavier_than_light_at_every_size() {
        // At a 1px stroke the only thing separating them is the doubling, and a
        // heavy rule that rasterizes identically to a light one is a silent
        // failure -- the text says heavy and the screen does not.
        for cell_h in [8u32, 12, 20, 40] {
            let c = CellMetrics { cell_h, ..cell() };
            let light = mask_of('─', c).data.iter().filter(|&&v| v > 0).count();
            let heavy = mask_of('━', c).data.iter().filter(|&&v| v > 0).count();
            assert!(heavy > light, "heavy must outweigh light at cell_h={cell_h}");
        }
    }

    #[test]
    fn dashes_are_the_one_thing_that_may_have_gaps() {
        // Everything else in this module exists to remove gaps; a dashed rule
        // is defined by them, so it must not be "fixed" by the tiling rule.
        let img = mask_of('┄', cell());
        let row = (0..img.height).find(|&y| (0..img.width).any(|x| at(&img, x, y) > 0)).unwrap();
        let covered = (0..img.width).filter(|&x| at(&img, x, row) > 0).count();
        assert!(covered > 0, "a dashed rule still draws something");
        assert!(covered < img.width as usize, "and it is still dashed");
    }

    #[test]
    fn eighths_tile_against_their_neighbours() {
        // Lower n/8 and upper (8-n)/8 must partition the cell exactly, or a
        // meter built from them shows a seam or overlaps by a pixel.
        let c = cell();
        let lower = mask_of('▄', c); // lower half
        let upper = mask_of('▀', c); // upper half
        for y in 0..c.cell_h {
            let a = at(&lower, 0, y) > 0;
            let b = at(&upper, 0, y) > 0;
            assert!(a != b, "row {y} is covered by both halves or by neither");
        }
    }

    #[test]
    fn quadrants_partition_the_cell(
    ) {
        // The four single quadrants together must cover every pixel exactly
        // once -- the same tiling requirement, in two dimensions.
        let c = cell();
        let q = ['▘', '▝', '▖', '▗'].map(|ch| mask_of(ch, c));
        for y in 0..c.cell_h {
            for x in 0..c.cell_w {
                let n = q.iter().filter(|img| at(img, x, y) > 0).count();
                assert_eq!(n, 1, "pixel {x},{y} is covered by {n} quadrants, not 1");
            }
        }
    }

    #[test]
    fn every_covered_codepoint_draws_something() {
        // `covers` is what the font path consults, so a codepoint it claims and
        // then declines to draw renders as nothing at all -- worse than tofu,
        // because tofu is visible.
        for cp in 0x2500..=0x259Fu32 {
            let ch = char::from_u32(cp).unwrap();
            assert!(covers(ch), "U+{cp:04X} should be claimed");
            let img = render(ch, cell()).unwrap_or_else(|| panic!("U+{cp:04X} claimed but not drawn"));
            assert!(
                img.data.iter().any(|&v| v > 0),
                "U+{cp:04X} draws an empty mask"
            );
        }
    }

    #[test]
    fn absurd_cells_do_not_panic() {
        // Cell geometry comes from user config by way of the font, and
        // `line_height = 0.01` is a thing a person can type.
        for (w, h, t) in [(1u32, 1u32, 1u32), (1, 40, 9), (40, 1, 9), (3, 3, 7), (200, 200, 1)] {
            let c = CellMetrics {
                cell_w: w,
                cell_h: h,
                baseline: h.saturating_sub(1),
                underline_y: h.saturating_sub(1),
                underline_thickness: t,
                strikeout_y: 0,
            };
            for cp in 0x2500..=0x259Fu32 {
                let ch = char::from_u32(cp).unwrap();
                let img = render(ch, c).expect("still drawn");
                assert_eq!(img.data.len(), (w * h) as usize);
            }
        }
    }

    #[test]
    fn arrows_are_left_to_the_font() {
        // They carry no tiling requirement and the font already has an opinion
        // about what an arrowhead looks like.
        assert!(!covers('\u{2190}'));
        assert!(!covers('\u{21FF}'));
    }
}
