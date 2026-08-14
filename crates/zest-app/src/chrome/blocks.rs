//! Block header chrome (design screen 3).
//!
//! Headers ride the scrollback: they sit on the grid rows a block's prompt
//! occupies, so unlike the cached [`super::layout`] pass this one is rebuilt
//! per frame from the visible rows. Still pure — the app extracts
//! [`BlockView`]s from the terminal under its own lock and this module only
//! does arithmetic, which is what makes it testable without a session.
//!
//! A header *replaces* the block's prompt rows visually (the shell's prompt
//! is underneath, painted over): the command comes back from the block index
//! in a compact, state-coloured form. The live prompt — a block still in
//! `Prompt` state — is never overlaid; that is where the user is typing.

use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::{ChromeHitMap, HitRegion};
use super::layout::TextRun;
use super::theme::ChromeColors;

/// One block's header, as data. Extracted by the app, drawn here.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockView {
    pub id: u32,
    /// Viewport rows the header covers: `[first, last)`, from the block's
    /// prompt line to its output line, clamped to what is visible.
    pub rows: (usize, usize),
    pub running: bool,
    pub failed: bool,
    /// Finished with no output at all — the rail goes faint.
    pub no_output: bool,
    pub command: String,
    pub cwd: String,
    /// "51.2s" — pre-formatted; empty when the host sent no stamps.
    pub duration: String,
    /// "exit 0" / "exit 1"; empty while running (the ring says it instead).
    pub exit_label: String,
    /// "running 4.2s"; empty unless running.
    pub running_label: String,
    pub folded: bool,
    /// Whether there is anything to fold.
    ///
    /// Must be the same predicate `block_actions::fold_row_map` folds on, or
    /// the header offers a chevron the fold cannot honour — which is what
    /// `cd ..` and every still-running command used to get.
    pub foldable: bool,
    /// Output lines hidden by the fold, for the "N lines" tag.
    pub folded_lines: usize,
    /// The host went away mid-run: rail faint, metadata says "interrupted".
    pub interrupted: bool,
}

/// The per-frame output: instances to append after the cached chrome, and a
/// hit map the app consults where the cached one says nothing.
#[derive(Debug, Default)]
pub struct BlockChrome {
    pub rects: Vec<RectInstance>,
    pub texts: Vec<TextRun>,
    pub hit: ChromeHitMap,
}

// Geometry, logical px (design screen 3). The band runs the full grid
// width — the design insets it 18px inside an empty pane gutter, but our
// band covers live prompt rows, and any inset lets their first characters
// peek out beside it.
const INSET: f32 = 0.0;
const RADIUS: f32 = 8.0;
const RAIL: f32 = 2.0;
const HPAD: f32 = 12.0;
const GAP: f32 = 10.0;
const CHIP_PAD: f32 = 7.0;
const CHIP_RADIUS: f32 = 5.0;
const META_PX: f32 = 11.0;
const CMD_PX: f32 = 12.5;
const CHIP_PX: f32 = 10.0;

fn baseline_in(y: f32, h: f32, px: f32) -> f32 {
    y + (h + px * 0.72) / 2.0
}

/// Lay the headers over the grid area `area`, rows of `cell_h` pixels.
/// `spin` is the running ring's rotation phase, 0..1 of a turn.
#[allow(clippy::too_many_arguments, reason = "one call site; a params struct would name nothing")]
pub fn layout_blocks(
    views: &[BlockView],
    area: [f32; 4],
    cell_h: f32,
    s: f32,
    colors: &ChromeColors,
    hover: Option<HitRegion>,
    spin: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> BlockChrome {
    let mut out = BlockChrome::default();
    let clip = area;

    for v in views {
        let (r0, r1) = v.rows;
        if r1 <= r0 {
            continue;
        }
        let band = [
            area[0] + INSET * s,
            area[1] + r0 as f32 * cell_h,
            area[2] - 2.0 * INSET * s,
            (r1 - r0) as f32 * cell_h,
        ];
        if band[1] >= area[1] + area[3] || band[1] + band[3] <= area[1] {
            continue;
        }

        let fill = if v.running { colors.panel_bg } else { colors.block_header_bg };
        out.rects.push(RectInstance::rounded(band, RADIUS * s, fill, clip));
        out.hit.push(band, HitRegion::BlockHeader(v.id));

        // The state rail. Radius 1: a 2px rect cannot honestly round more.
        let rail_color = if v.interrupted {
            colors.text_faint
        } else if v.running {
            colors.warn
        } else if v.failed {
            colors.danger
        } else if v.no_output {
            colors.text_faint
        } else {
            colors.success
        };
        out.rects.push(RectInstance::rounded(
            [band[0], band[1], RAIL * s, band[3]],
            1.0 * s,
            rail_color,
            clip,
        ));

        // Fold chevron. Its hit region is wider than its glyph — a 6px
        // triangle is not a target. Drawn only when there is output to hide:
        // an affordance that answers a click by doing nothing is worse than
        // none. The command text's x-offset is unaffected either way, so
        // labels stay aligned down the column.
        // Outside the branch: the command text starts past the chevron's slot
        // whether or not one is drawn, so labels stay aligned down the column.
        let chev_x = band[0] + HPAD * s;
        if v.foldable {
            let chev = if v.folded { "\u{25b8}" } else { "\u{25be}" };
            out.hit.push(
                [band[0], band[1], HPAD * s + 16.0 * s, band[3]],
                HitRegion::BlockFold(v.id),
            );
            out.texts.push(TextRun {
                text: chev.into(),
                pos: [chev_x, baseline_in(band[1], band[3], CMD_PX * s)],
                max_width: 16.0 * s,
                color: colors.text_faint,
                clip,
                px: CMD_PX * s,
                bold: false,
                tracking: 0.0,
            });
        }

        // Right-to-left: chips on hover, then outcome, duration, cwd.
        let mut right = band[0] + band[2] - HPAD * s;
        let hovered = matches!(
            hover,
            Some(
                HitRegion::BlockHeader(h)
                    | HitRegion::BlockFold(h)
                    | HitRegion::BlockCopy(h)
                    | HitRegion::BlockRerun(h)
            ) if h == v.id
        );
        if hovered {
            for (label, region) in [
                ("re-run \u{2318}\u{21e7}R", HitRegion::BlockRerun(v.id)),
                ("copy output \u{2318}\u{21e7}O", HitRegion::BlockCopy(v.id)),
            ] {
                let w = measure(label, CHIP_PX * s, false, 0.0) + 2.0 * CHIP_PAD * s;
                let chip_h = 16.0 * s;
                let chip = [right - w, band[1] + (band[3] - chip_h) / 2.0, w, chip_h];
                out.rects.push(RectInstance::rounded(chip, CHIP_RADIUS * s, colors.accent_soft, clip));
                out.hit.push(chip, region);
                out.texts.push(TextRun {
                    text: label.into(),
                    pos: [chip[0] + CHIP_PAD * s, baseline_in(chip[1], chip[3], CHIP_PX * s)],
                    max_width: w,
                    color: colors.accent,
                    clip,
                    px: CHIP_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                right = chip[0] - 5.0 * s;
            }
        }

        let mut meta = |text: &str, color: LinearRgba, right: &mut f32, out: &mut BlockChrome| {
            if text.is_empty() {
                return;
            }
            let w = measure(text, META_PX * s, false, 0.0);
            out.texts.push(TextRun {
                text: text.into(),
                pos: [*right - w, baseline_in(band[1], band[3], META_PX * s)],
                max_width: w + 2.0,
                color,
                clip,
                px: META_PX * s,
                bold: false,
                tracking: 0.0,
            });
            *right -= w + GAP * s;
        };

        if v.interrupted {
            meta("interrupted", colors.text_faint, &mut right, &mut out);
        } else if v.running {
            // The running indicator: a thin warn ring whose gap orbits on the
            // clock's 0.9s turn — an SDF box cannot draw an arc, but a ring
            // with a fill-coloured bite out of it reads exactly the same.
            meta(&v.running_label, colors.warn, &mut right, &mut out);
            let d = 8.0 * s;
            let ring = [right - d, band[1] + (band[3] - d) / 2.0, d, d];
            out.rects.push(RectInstance {
                radii: [d / 2.0; 4],
                border: colors.warn,
                border_width: 1.5 * s,
                ..RectInstance::filled(ring, LinearRgba::TRANSPARENT, clip)
            });
            let angle = spin * core::f32::consts::TAU;
            let r = d / 2.0;
            let (cx, cy) = (ring[0] + r, ring[1] + r);
            let bite = 3.0 * s;
            out.rects.push(RectInstance::rounded(
                [
                    cx + angle.cos() * r - bite / 2.0,
                    cy + angle.sin() * r - bite / 2.0,
                    bite,
                    bite,
                ],
                bite / 2.0,
                fill,
                clip,
            ));
            right -= d + GAP * s;
        } else {
            let color = if v.failed { colors.danger } else { colors.success };
            meta(&v.exit_label, color, &mut right, &mut out);
        }
        meta(&v.duration, colors.text_faint, &mut right, &mut out);
        if v.folded && v.folded_lines > 0 {
            let lines = format!("{} lines", v.folded_lines);
            meta(&lines, colors.text_faint, &mut right, &mut out);
        }
        meta(&v.cwd, colors.text_faint, &mut right, &mut out);

        // The command, in the room that remains.
        let cmd_x = chev_x + 16.0 * s;
        let cmd_color = if v.interrupted {
            colors.text_inactive
        } else if v.running {
            colors.text_active
        } else if v.failed {
            colors.danger
        } else if v.no_output {
            colors.text_inactive
        } else {
            colors.success
        };
        out.texts.push(TextRun {
            text: v.command.clone(),
            pos: [cmd_x, baseline_in(band[1], band[3], CMD_PX * s)],
            max_width: (right - cmd_x).max(0.0),
            color: cmd_color,
            clip,
            px: CMD_PX * s,
            bold: false,
            tracking: 0.0,
        });
    }

    out
}

/// "51.2s", "0.41s", "4m 12s" — a duration as the design prints one.
#[must_use]
pub fn format_duration(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 10.0 {
        format!("{secs:.2}s")
    } else if secs < 100.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0) as u64;
        let rem = secs as u64 % 60;
        format!("{m}m {rem:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0)
    }

    fn measure(s: &str, px: f32, _bold: bool, _tracking: f32) -> f32 {
        s.chars().count() as f32 * px * 0.6
    }

    fn view(id: u32, rows: (usize, usize)) -> BlockView {
        BlockView {
            id,
            rows,
            running: false,
            failed: false,
            no_output: false,
            command: "cargo build".into(),
            cwd: "~/dev".into(),
            duration: "51.2s".into(),
            exit_label: "exit 0".into(),
            running_label: String::new(),
            folded: false,
            foldable: true,
            folded_lines: 0,
            interrupted: false,
        }
    }

    #[test]
    fn a_header_answers_as_itself_and_the_chevron_wins_inside_it() {
        // The hit-map discipline, applied to blocks: the band is one region,
        // the fold affordance another, and the fold is on top where drawn.
        let area = [0.0, 100.0, 800.0, 400.0];
        let b = layout_blocks(&[view(7, (2, 3))], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        let band_y = 100.0 + 2.0 * 20.0 + 10.0;
        assert_eq!(b.hit.hit(400.0, band_y), Some(HitRegion::BlockHeader(7)));
        assert_eq!(
            b.hit.hit(INSET + 4.0, band_y),
            Some(HitRegion::BlockFold(7)),
            "the chevron zone outranks the band it sits in"
        );
    }

    #[test]
    fn the_band_the_layout_draws_routes_the_wheel_to_the_grid() {
        // #256, closed at both ends. `hit.rs` pins what each *region* means;
        // this pins that the rectangle actually drawn answers with one of
        // those regions — the property that broke, since the band is pushed
        // at full grid width and the wheel asked only whether *anything* was
        // hit. Feed the real layout's answer through the real classifier.
        let area = [0.0, 100.0, 800.0, 400.0];
        let b = layout_blocks(&[view(7, (2, 3))], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        let band_y = 100.0 + 2.0 * 20.0 + 10.0;
        for x in [INSET + 4.0, 400.0, 795.0] {
            assert_eq!(
                super::super::hit::wheel_target(b.hit.hit(x, band_y), None),
                super::super::hit::WheelTarget::Grid,
                "x={x} is over a header the layout drew inside the grid, so the wheel scrolls the session"
            );
        }
    }

    #[test]
    fn a_block_with_nothing_to_fold_offers_no_chevron() {
        // `cd ..` printed nothing and a running command has not finished, so
        // `fold_row_map` declines both. The header used to draw a chevron and
        // claim the click anyway, and tapping it did nothing at all.
        let area = [0.0, 100.0, 800.0, 400.0];
        let v = BlockView { foldable: false, ..view(7, (2, 3)) };
        let b = layout_blocks(&[v], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        let band_y = 100.0 + 2.0 * 20.0 + 10.0;
        assert_eq!(
            b.hit.hit(INSET + 4.0, band_y),
            Some(HitRegion::BlockHeader(7)),
            "the chevron zone must fall through to the band when nothing folds"
        );
        assert!(
            !b.texts.iter().any(|t| t.text == "\u{25be}" || t.text == "\u{25b8}"),
            "no chevron glyph is drawn for a block that cannot fold"
        );
    }

    #[test]
    fn hover_grows_the_action_chips_and_they_answer() {
        let area = [0.0, 0.0, 800.0, 400.0];
        let quiet =
            layout_blocks(&[view(3, (0, 1))], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        let hovered = layout_blocks(
            &[view(3, (0, 1))],
            area,
            20.0,
            1.0,
            &colors(),
            Some(HitRegion::BlockHeader(3)),
            0.0,
            &mut measure,
        );
        assert!(
            hovered.rects.len() > quiet.rects.len(),
            "hover must reveal the chips"
        );
        let found = (0..800).step_by(2).any(|x| {
            (0..20).step_by(2).any(|y| {
                matches!(
                    hovered.hit.hit(x as f32, y as f32),
                    Some(HitRegion::BlockCopy(3) | HitRegion::BlockRerun(3))
                )
            })
        });
        assert!(found, "the chips must be clickable where they are drawn");
    }

    #[test]
    fn hovering_a_chip_keeps_the_chips_drawn() {
        // What bounds `App::revalidate_hover`. It re-reads hover against the
        // map each frame, and the chips exist in that map only while the
        // block is hovered — so landing on one goes None → BlockHeader →
        // BlockCopy over two frames. It stops there *because* of this: every
        // region that reveals the chips also keeps them revealed. Were
        // `hovered` to drop the chip regions from its arm, the chips would
        // vanish, hover would fall back to the band, and the two would
        // alternate for ever at one frame each — a terminal repainting at
        // full rate with nothing on screen changing, which the 0%-idle
        // guarantee exists to prevent.
        let area = [0.0, 0.0, 800.0, 400.0];
        let counts: Vec<usize> = [
            HitRegion::BlockHeader(3),
            HitRegion::BlockFold(3),
            HitRegion::BlockCopy(3),
            HitRegion::BlockRerun(3),
        ]
        .into_iter()
        .map(|h| {
            layout_blocks(&[view(3, (0, 1))], area, 20.0, 1.0, &colors(), Some(h), 0.0, &mut measure)
                .rects
                .len()
        })
        .collect();
        assert!(
            counts.iter().all(|n| *n == counts[0]),
            "all four block regions must draw the same chrome, or hover oscillates: {counts:?}"
        );
    }

    #[test]
    fn durations_print_at_the_designs_precision() {
        assert_eq!(format_duration(410), "0.41s");
        assert_eq!(format_duration(51_200), "51.2s");
        assert_eq!(format_duration(252_000), "4m 12s");
    }
}
