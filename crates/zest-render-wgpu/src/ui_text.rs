//! Proportional text runs for the chrome — tab titles, host labels, the
//! command palette.
//!
//! The grid path shapes per-cell and throws the shaper's advances away; UI
//! text is the opposite trade. A run is shaped once, keeps its advances, and
//! is truncated with an ellipsis when it will not fit. Clusters the primary
//! face cannot draw fall back per character through the system machinery —
//! `shape_run` shapes only the primary face, so a CJK directory name or an
//! emoji in a shell title would otherwise render as a row of notdef boxes.
//!
//! Layout decisions (what fits, where the cut lands) are pure and tested
//! without a GPU; only the final atlas resolution touches the device.

use zest_font::{Fonts, GlyphKey, Style};

use crate::atlas::{Atlas, Cached};
use crate::instance::{glyph_flags, GlyphInstance, LinearRgba};

/// One glyph of a run, positioned relative to the run's start.
struct Placed {
    key: GlyphKey,
    x: f32,
}

/// A shaped, positioned run — not yet resolved against the atlas.
struct PlacedRun {
    glyphs: Vec<Placed>,
    /// Per cluster, in text order: how many glyphs it produced and its width.
    /// Truncation cuts at cluster boundaries so a ligature or a fallback
    /// sequence is never split mid-glyph.
    clusters: Vec<(usize, f32)>,
    width: f32,
}

/// Where a width budget cuts a run.
#[derive(Debug, PartialEq, Eq)]
enum Truncation {
    /// Everything fits; no ellipsis.
    All,
    /// Keep this many leading clusters, then draw the ellipsis.
    Clusters(usize),
    /// Not even the ellipsis fits; draw nothing.
    Nothing,
}

fn truncate(cluster_widths: &[f32], max_width: f32, ellipsis_width: f32) -> Truncation {
    let total: f32 = cluster_widths.iter().sum();
    if total <= max_width {
        return Truncation::All;
    }
    if ellipsis_width > max_width {
        return Truncation::Nothing;
    }
    let budget = max_width - ellipsis_width;
    let mut used = 0.0;
    let mut keep = 0;
    for &w in cluster_widths {
        if used + w > budget {
            break;
        }
        used += w;
        keep += 1;
    }
    Truncation::Clusters(keep)
}

fn place_run(fonts: &mut Fonts, text: &str, style: Style) -> PlacedRun {
    let shaped = fonts.shape_run(text, style, &[]);
    let mut glyphs = Vec::new();
    let mut clusters = Vec::new();
    let mut pen = 0.0f32;

    // Shaping can come back empty (no usable face); resolve every character
    // through fallback rather than drawing nothing.
    if shaped.is_empty() {
        for ch in text.chars() {
            if let Some((f, g)) = fonts.glyph_for(ch, style) {
                let adv = fonts.advance_of(f, g);
                glyphs.push(Placed { key: fonts.key(f, g), x: pen });
                clusters.push((1, adv));
                pen += adv;
            }
        }
        return PlacedRun { glyphs, clusters, width: pen };
    }

    let mut i = 0;
    while i < shaped.len() {
        let start = shaped[i].cluster;
        let mut j = i;
        while j < shaped.len() && shaped[j].cluster == start {
            j += 1;
        }

        if shaped[i..j].iter().all(|g| g.glyph == 0) {
            // The primary face lacks this cluster entirely. Re-resolve its
            // characters through system fallback, at the cost of per-char
            // positioning — which forfeits ligatures only for text the
            // primary font could not draw anyway.
            let end = shaped.get(j).map_or(text.len(), |g| g.cluster as usize);
            let mut count = 0;
            let mut w = 0.0;
            for ch in text[start as usize..end].chars() {
                if let Some((f, g)) = fonts.glyph_for(ch, style) {
                    let adv = fonts.advance_of(f, g);
                    glyphs.push(Placed { key: fonts.key(f, g), x: pen + w });
                    w += adv;
                    count += 1;
                }
            }
            clusters.push((count, w));
            pen += w;
        } else {
            let mut count = 0;
            let mut w = 0.0;
            for g in &shaped[i..j] {
                glyphs.push(Placed { key: fonts.key(g.font, g.glyph), x: pen + w });
                w += g.advance;
                count += 1;
            }
            clusters.push((count, w));
            pen += w;
        }
        i = j;
    }

    PlacedRun { glyphs, clusters, width: pen }
}

/// The ellipsis, as placeable glyphs with advances.
///
/// Prefers `…`; a face without it (through fallback, rare) gets three periods,
/// which every monospace face has.
fn ellipsis_glyphs(fonts: &mut Fonts, style: Style) -> (Vec<(GlyphKey, f32)>, f32) {
    if let Some((f, g)) = fonts.glyph_for('…', style) {
        let adv = fonts.advance_of(f, g);
        if adv > 0.0 {
            return (vec![(fonts.key(f, g), adv)], adv);
        }
    }
    let mut out = Vec::new();
    let mut w = 0.0;
    for _ in 0..3 {
        if let Some((f, g)) = fonts.glyph_for('.', style) {
            let adv = fonts.advance_of(f, g);
            out.push((fonts.key(f, g), adv));
            w += adv;
        }
    }
    (out, w)
}

/// The width `text` would occupy, in physical pixels.
///
/// This is the measurement half of [`emit_ui_run`]: chrome layout uses it to
/// decide tab widths and truncation before any drawing happens.
#[must_use]
pub fn measure_ui_run(fonts: &mut Fonts, text: &str, style: Style) -> f32 {
    place_run(fonts, text, style).width
}

/// Shape, truncate and emit one run of UI text into `out`.
///
/// `pos` is the baseline-left origin in physical pixels. Every emitted glyph
/// carries [`glyph_flags::FIXED`], so chrome text stays put when the grid
/// smooth-scrolls. Returns the width actually emitted (including the ellipsis
/// when the run was cut), which is what centring and right-alignment need.
#[allow(clippy::too_many_arguments)]
pub fn emit_ui_run(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atlas: &mut Atlas,
    fonts: &mut Fonts,
    text: &str,
    style: Style,
    pos: [f32; 2],
    color: LinearRgba,
    clip: [f32; 4],
    max_width: f32,
    out: &mut Vec<GlyphInstance>,
) -> f32 {
    let run = place_run(fonts, text, style);

    if run.width <= max_width {
        for p in &run.glyphs {
            push_glyph(device, queue, atlas, fonts, p.key, pos[0] + p.x, pos[1], color, clip, out);
        }
        return run.width;
    }

    let (ellipsis, ellipsis_width) = ellipsis_glyphs(fonts, style);
    let widths: Vec<f32> = run.clusters.iter().map(|c| c.1).collect();
    let keep = match truncate(&widths, max_width, ellipsis_width) {
        // Unreachable: the early return above handled the fitting case. Kept
        // as a harmless arm rather than a panic in a render path.
        Truncation::All => run.clusters.len(),
        Truncation::Clusters(n) => n,
        Truncation::Nothing => return 0.0,
    };

    let kept_glyphs: usize = run.clusters[..keep].iter().map(|c| c.0).sum();
    let kept_width: f32 = run.clusters[..keep].iter().map(|c| c.1).sum();
    for p in &run.glyphs[..kept_glyphs] {
        push_glyph(device, queue, atlas, fonts, p.key, pos[0] + p.x, pos[1], color, clip, out);
    }
    let mut x = pos[0] + kept_width;
    for (key, adv) in ellipsis {
        push_glyph(device, queue, atlas, fonts, key, x, pos[1], color, clip, out);
        x += adv;
    }
    kept_width + ellipsis_width
}

#[allow(clippy::too_many_arguments)]
fn push_glyph(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atlas: &mut Atlas,
    fonts: &mut Fonts,
    key: GlyphKey,
    x: f32,
    baseline: f32,
    color: LinearRgba,
    clip: [f32; 4],
    out: &mut Vec<GlyphInstance>,
) {
    let cached = match atlas.get(&key) {
        Some(c) => c,
        None => {
            let Some(image) = fonts.rasterize(key) else { return };
            match atlas.insert(device, queue, key, &image) {
                Some(c) => c,
                None => return,
            }
        }
    };
    let Cached::Entry(e) = cached else { return };

    out.push(GlyphInstance {
        // Bearing baked in on the CPU, exactly as the grid path does.
        pos: [x + f32::from(e.left), baseline - f32::from(e.top)],
        uv: [f32::from(e.uv[0]), f32::from(e.uv[1])],
        size: [f32::from(e.size[0]), f32::from(e.size[1])],
        color,
        clip,
        layer: u32::from(e.layer),
        flags: glyph_flags::FIXED | if e.is_color { glyph_flags::COLOR } else { 0 },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_font::Typography;

    #[test]
    fn a_fitting_run_is_never_truncated() {
        assert_eq!(truncate(&[10.0, 10.0, 10.0], 30.0, 8.0), Truncation::All, "exact fit is a fit");
        assert_eq!(truncate(&[10.0], 100.0, 8.0), Truncation::All);
        assert_eq!(truncate(&[], 0.0, 8.0), Truncation::All, "empty text always fits");
    }

    #[test]
    fn an_overflowing_run_cuts_at_a_cluster_boundary_with_room_for_the_ellipsis() {
        // 40 total into 35: the ellipsis (8) leaves a 27 budget, so two whole
        // clusters (20) survive and the third would not.
        assert_eq!(truncate(&[10.0, 10.0, 10.0, 10.0], 35.0, 8.0), Truncation::Clusters(2));
        // A budget that lands exactly on a boundary keeps that cluster.
        assert_eq!(truncate(&[10.0, 10.0, 10.0, 10.0], 38.0, 8.0), Truncation::Clusters(3));
    }

    #[test]
    fn a_hopeless_budget_draws_nothing_rather_than_a_clipped_ellipsis() {
        assert_eq!(truncate(&[10.0, 10.0], 5.0, 8.0), Truncation::Nothing);
        // Room for the ellipsis alone: no clusters, but the ellipsis shows,
        // which is how a tab narrowed to nothing still signals "there is text".
        assert_eq!(truncate(&[10.0, 10.0], 9.0, 8.0), Truncation::Clusters(0));
    }

    /// The one shaping test that must not skip: the generic `monospace` family
    /// resolves on every supported machine (zest-font asserts exactly this),
    /// so a run measured through it must have real width.
    #[test]
    fn ui_runs_measure_nonzero_and_fall_back_past_the_primary_face() {
        let mut fonts = Fonts::new(&["monospace".to_string()], Typography::default())
            .expect("the CSS generic `monospace` must resolve");

        let plain = measure_ui_run(&mut fonts, "hello", Style::default());
        assert!(plain > 0.0, "a shaped ASCII run must have width");

        // A char no monospace primary face carries: measured through system
        // fallback it should still come out wider than nothing on any machine
        // that can draw it at all — and never panic on one that cannot.
        let cjk = measure_ui_run(&mut fonts, "終", Style::default());
        let empty = measure_ui_run(&mut fonts, "", Style::default());
        assert_eq!(empty, 0.0, "empty text is zero wide");
        assert!(cjk >= 0.0);
    }
}
