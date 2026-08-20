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

use zest_render_wgpu::{border_sides, LinearRgba, RectInstance};

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
    /// Viewport rows the *whole* block covers, `[first, last)`, clipped —
    /// header and output together. Only the click target: the rail itself is
    /// drawn a layer down, in the grid, because chrome paints over the glyphs
    /// (see `zest_render_wgpu::BlockBand`).
    pub hit_rows: (usize, usize),
    /// This block is the pane's selection: accent rail, accentSoft band.
    pub selected: bool,
    /// Its ⋯ menu is open, so the affordance stays drawn even though the
    /// pointer has left the header for the menu.
    pub menu_open: bool,
}

/// The per-frame output: instances to append after the cached chrome, and a
/// hit map the app consults where the cached one says nothing.
#[derive(Debug, Default)]
pub struct BlockChrome {
    pub rects: Vec<RectInstance>,
    pub texts: Vec<TextRun>,
    pub hit: ChromeHitMap,
    /// The `⋯` rect this pass drew, and whose block. The menu hangs off it, so
    /// the affordance and its menu come from one computation and cannot drift.
    pub menu_anchor: Option<(u32, [f32; 4])>,
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
const CHIP_RADIUS: f32 = 5.0;
/// The ⋯ affordance's square.
const DOTS: f32 = 16.0;
/// The rail's click strip. Wider than the 2px rule, because a 2px target is
/// not a target — but well under half a cell, so the first character of an
/// output row stays selectable by a press a hair to its right.
const RAIL_HIT: f32 = 8.0;
const META_PX: f32 = 11.0;
const CMD_PX: f32 = 12.5;

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

        // The rail strip first, over the block's whole height, so the band
        // pushed after it wins inside the header rows: push order is draw
        // order, and the lookup walks it backwards.
        let (e0, e1) = v.hit_rows;
        if e1 > e0 {
            out.hit.push(
                [area[0], area[1] + e0 as f32 * cell_h, RAIL_HIT * s, (e1 - e0) as f32 * cell_h],
                HitRegion::BlockRail(v.id),
            );
        }

        let fill = if v.selected {
            colors.accent_soft
        } else if v.running {
            colors.panel_bg
        } else {
            colors.block_header_bg
        };
        let rail_color = if v.selected {
            colors.accent
        } else if v.interrupted {
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
        // The state rail is the header's own left border, not a rect beside it:
        // the mock is `border-radius:8px; border-left:2px solid <state>` on one
        // box, and CSS thins that stroke to nothing as the corner arc turns into
        // the zero-width top border. On a header barely taller than two radii
        // the rail is therefore mostly curve — it reads as `(`, not as a bar,
        // and a separate rect cannot produce that at any radius.
        out.rects.push(RectInstance {
            radii: [RADIUS * s; 4],
            border: rail_color,
            border_width: RAIL * s,
            border_omit: border_sides::TOP | border_sides::RIGHT | border_sides::BOTTOM,
            ..RectInstance::filled(band, fill, clip)
        });
        out.hit.push(band, HitRegion::BlockHeader(v.id));

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

        // Right-to-left: the ⋯ slot, then outcome, duration, cwd.
        //
        // The slot is reserved whether or not the affordance is drawn, so the
        // metadata does not slide sideways when the pointer arrives — the rule
        // the chevron's comment already states for the left edge. The two
        // chips this replaced did jump, every time.
        let mut right = band[0] + band[2] - HPAD * s;
        let dots = [right - DOTS * s, band[1] + (band[3] - DOTS * s) / 2.0, DOTS * s, DOTS * s];
        right = dots[0] - GAP * s;
        let hovered = matches!(
            hover,
            Some(
                HitRegion::BlockHeader(h)
                    | HitRegion::BlockFold(h)
                    | HitRegion::BlockRail(h)
                    | HitRegion::BlockMenu(h)
            ) if h == v.id
        );
        // Every region that reveals it also keeps it revealed, and the menu
        // being open holds it there while the pointer is off in the menu.
        // Without that, hover would alternate frame by frame — the affordance
        // vanishing moves the pointer back onto the band, which reveals it
        // again (`App::revalidate_hover`, #256).
        if hovered || v.menu_open {
            out.rects.push(RectInstance::rounded(dots, CHIP_RADIUS * s, colors.accent_soft, clip));
            out.hit.push(dots, HitRegion::BlockMenu(v.id));
            let label = "\u{22ef}";
            let w = measure(label, CMD_PX * s, false, 0.0);
            out.texts.push(TextRun {
                text: label.into(),
                pos: [dots[0] + (dots[2] - w) / 2.0, baseline_in(dots[1], dots[3], CMD_PX * s)],
                max_width: dots[2],
                color: colors.accent,
                clip,
                px: CMD_PX * s,
                bold: false,
                tracking: 0.0,
            });
            out.menu_anchor = Some((v.id, dots));
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
            // clock's 0.9s turn. Shared with the tab chips since #385 — same
            // picture, and the `fill` argument is why it had to become a
            // function: a chip's background is not a header's.
            meta(&v.running_label, colors.warn, &mut right, &mut out);
            let d = 8.0 * s;
            super::layout::ring(
                &mut out.rects,
                [right - d, band[1] + (band[3] - d) / 2.0, d, d],
                colors.warn,
                fill,
                super::layout::RingStyle::Spin(spin),
                clip,
            );
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
            hit_rows: rows,
            selected: false,
            menu_open: false,
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
    fn hover_reveals_the_menu_affordance_and_it_answers() {
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
        assert!(hovered.rects.len() > quiet.rects.len(), "hover must reveal the affordance");
        let found = (0..800).step_by(2).any(|x| {
            (0..20)
                .step_by(2)
                .any(|y| hovered.hit.hit(x as f32, y as f32) == Some(HitRegion::BlockMenu(3)))
        });
        assert!(found, "the affordance must be clickable where it is drawn");
        assert_eq!(
            hovered.menu_anchor.map(|(id, _)| id),
            Some(3),
            "and it must report where it drew, so the menu hangs off the same rect"
        );
    }

    #[test]
    fn the_metadata_does_not_move_when_the_affordance_appears() {
        // The two chips this replaced were laid out only when hovered, so the
        // right-aligned metadata slid sideways every time the pointer arrived
        // — the exact jitter the chevron's slot is reserved to prevent on the
        // left. The slot is now reserved whether or not anything fills it.
        let area = [0.0, 0.0, 800.0, 400.0];
        let x_of = |hover| {
            layout_blocks(&[view(3, (0, 1))], area, 20.0, 1.0, &colors(), hover, 0.0, &mut measure)
                .texts
                .iter()
                .find(|t| t.text == "exit 0")
                .map(|t| t.pos[0])
        };
        assert_eq!(
            x_of(None),
            x_of(Some(HitRegion::BlockHeader(3))),
            "the outcome label must not move under the pointer"
        );
    }

    #[test]
    fn hovering_the_affordance_keeps_it_drawn() {
        // What bounds `App::revalidate_hover`. It re-reads hover against the
        // map each frame, and the ⋯ exists in that map only while the block is
        // hovered — so landing on it goes None → BlockHeader → BlockMenu over
        // two frames. It stops there *because* of this: every region that
        // reveals the affordance also keeps it revealed. Narrow that arm and
        // the ⋯ vanishes, hover falls back to the band, and the two alternate
        // for ever at one frame each — a terminal repainting at full rate with
        // nothing on screen changing, which is the exact failure the 0%-idle
        // guarantee exists to prevent.
        let area = [0.0, 0.0, 800.0, 400.0];
        let counts: Vec<usize> = [
            HitRegion::BlockHeader(3),
            HitRegion::BlockFold(3),
            HitRegion::BlockRail(3),
            HitRegion::BlockMenu(3),
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
    fn an_open_menu_holds_its_affordance_without_the_pointer() {
        // The pointer is off in the menu, not on the header. Without this the
        // ⋯ blinks out the moment the menu it opened appears.
        let area = [0.0, 0.0, 800.0, 400.0];
        let v = BlockView { menu_open: true, ..view(3, (0, 1)) };
        let out = layout_blocks(&[v], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        assert_eq!(out.menu_anchor.map(|(id, _)| id), Some(3));
    }

    #[test]
    fn the_rail_strip_makes_the_whole_block_clickable_and_the_band_still_wins() {
        // The click target the one-row header never was: a block's *extent*.
        // The band is pushed after the rail, so the header rows still answer
        // as the header — push order is draw order and the lookup walks it
        // backwards.
        let area = [0.0, 100.0, 800.0, 400.0];
        let v = BlockView { hit_rows: (2, 9), ..view(7, (2, 3)) };
        let b = layout_blocks(&[v], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        let band_y = 100.0 + 2.0 * 20.0 + 10.0;
        let body_y = 100.0 + 6.0 * 20.0 + 10.0;
        assert_eq!(
            b.hit.hit(1.0, body_y),
            Some(HitRegion::BlockRail(7)),
            "the gutter beside the output selects the block"
        );
        assert_eq!(
            b.hit.hit(1.0, band_y),
            Some(HitRegion::BlockFold(7)),
            "inside the header the band's own affordances still win"
        );
        assert_eq!(
            b.hit.hit(400.0, body_y),
            None,
            "and the output itself stays the grid's, or text is unselectable"
        );
    }

    #[test]
    fn a_selected_block_takes_the_accent_without_moving_anything() {
        // Selection is a colour change, not a layout change: the same rects in
        // the same places, so lighting a block cannot shift the text under it.
        let area = [0.0, 0.0, 800.0, 400.0];
        let plain =
            layout_blocks(&[view(3, (0, 1))], area, 20.0, 1.0, &colors(), None, 0.0, &mut measure);
        let lit = layout_blocks(
            &[BlockView { selected: true, ..view(3, (0, 1)) }],
            area,
            20.0,
            1.0,
            &colors(),
            None,
            0.0,
            &mut measure,
        );
        assert_eq!(plain.rects.len(), lit.rects.len(), "no rect appears or leaves");
        let c = colors();
        assert!(
            lit.rects.iter().any(|r| r.fill == c.accent_soft),
            "the selected band takes accentSoft"
        );
        // `border`, not `fill`: the header's rail is the band's own left
        // border rather than a rect beside it — see the comment where it is
        // drawn for why a separate rect cannot produce that shape.
        assert!(lit.rects.iter().any(|r| r.border == c.accent), "and its rail the accent");
    }

    #[test]
    fn durations_print_at_the_designs_precision() {
        assert_eq!(format_duration(410), "0.41s");
        assert_eq!(format_duration(51_200), "51.2s");
        assert_eq!(format_duration(252_000), "4m 12s");
    }

    #[test]
    fn the_rail_is_the_headers_own_left_border() {
        // The rail used to be a second rect laid over the band's left edge,
        // which is a straight bar with square ends however it is rounded — a
        // 2px-wide box cannot taper into an 8px corner. The mock is one box
        // with `border-left`, and the taper is the browser's, so the rail has
        // to be this box's border or it cannot look right.
        let area = [0.0, 100.0, 800.0, 400.0];
        let b = layout_blocks(&[view(7, (2, 3))], area, 20.0, 2.0, &colors(), None, 0.0, &mut measure);
        let band = [0.0, 100.0 + 2.0 * 20.0, 800.0, 20.0];
        let header: Vec<_> = b.rects.iter().filter(|r| r.rect == band).collect();
        assert_eq!(header.len(), 1, "one box, not a band plus a rail beside it");
        let h = header[0];
        assert_eq!(h.border_width, RAIL * 2.0, "the rail is 2 logical px, scaled");
        assert_eq!(h.radii, [RADIUS * 2.0; 4], "all four corners round, as `border-radius:8px` does");
        assert_eq!(
            h.border_omit,
            border_sides::TOP | border_sides::RIGHT | border_sides::BOTTOM,
            "left only — the other three sides are what the stroke tapers into"
        );
        assert_eq!(h.border, colors().success, "exit 0 inks the rail green");
        assert_eq!(h.fill, colors().block_header_bg);
    }

    #[test]
    fn every_block_state_reaches_the_rail() {
        // Read off the header's border rather than a second rect: the states
        // are the only thing the rail says, so losing one is silent.
        let area = [0.0, 0.0, 800.0, 400.0];
        let c = colors();
        let rail = |v: BlockView| {
            let b = layout_blocks(&[v], area, 20.0, 1.0, &c, None, 0.0, &mut measure);
            b.rects.iter().find(|r| r.border_width > 0.0 && r.rect[2] == 800.0).expect("a header").border
        };
        assert_eq!(rail(view(1, (0, 1))), c.success, "exit 0");
        assert_eq!(rail(BlockView { failed: true, ..view(1, (0, 1)) }), c.danger, "non-zero exit");
        assert_eq!(rail(BlockView { running: true, ..view(1, (0, 1)) }), c.warn, "still running");
        assert_eq!(rail(BlockView { no_output: true, ..view(1, (0, 1)) }), c.text_faint, "printed nothing");
        assert_eq!(rail(BlockView { interrupted: true, ..view(1, (0, 1)) }), c.text_faint, "the host went away");
    }
}
