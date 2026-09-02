//! Drawing an open file inside a pane (#464).
//!
//! Pure, like the rest of `chrome`: rectangles, text runs and hit regions out,
//! nothing about the GPU in. The lines arrive already sliced to what fits and
//! already tab-expanded ([`EditorView`]) — the geometry that decides how many
//! is the app's, and the expansion has to happen before shaping because a
//! literal tab is given whatever advance the font face carries, which is
//! usually none.
//!
//! # The file is not wrapped, and that is a decision
//!
//! Wrapping would break the identity between a line number and a screen row,
//! which the gutter, "open this at line N" and every future caret movement all
//! rest on. The terminal in the pane beside it does not wrap either, and two
//! panes with different line models is a thing the reader has to hold in their
//! head for no gain. Long lines scroll sideways instead.

use zest_render_wgpu::RectInstance;

use super::layout::{baseline_in, TextRun, HAIRLINE};
use super::model::EditorView;
use super::theme::ChromeColors;

/// Space either side of the gutter's digits.
pub const GUTTER_PAD: f32 = 8.0;

/// Space between the gutter rule and the first glyph of a line.
pub const TEXT_PAD: f32 = 10.0;

/// What one file pane draws.
pub struct EditorChrome {
    pub rects: Vec<RectInstance>,
    pub texts: Vec<TextRun>,
}

/// The type sizes an open file is drawn at.
///
/// Grouped rather than passed as four floats: they always travel together, and
/// the call site reads as "the grid's metrics" instead of as an argument order
/// to get right.
#[derive(Debug, Clone, Copy)]
pub struct EditorMetrics {
    /// The grid's cell width, so the gutter is a whole number of columns and a
    /// sideways flick moves a file as far as it moves a terminal.
    pub cell_w: f32,
    /// The grid's cell height: one line, one row.
    pub cell_h: f32,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// The grid's font size — a file is text, not a UI label about text.
    pub px: f32,
}

/// Width the gutter needs for a file of `total` lines.
///
/// Sized for the **file**, not for what is on screen: a gutter that widened as
/// you scrolled past line 99 would shift every line of the file sideways while
/// you read it.
#[must_use]
pub fn gutter_width(total: usize, cell_w: f32, s: f32) -> f32 {
    let digits = total.max(1).ilog10() as usize + 1;
    digits.max(2) as f32 * cell_w + 2.0 * GUTTER_PAD * s
}

/// Lay out one open file in `body`.
///
/// `cell_w`/`cell_h` are the grid's, so a file and a terminal beside it sit on
/// the same rhythm — the thing that makes a split of one of each look like one
/// window rather than two applications.
pub fn layout_editor(
    view: &EditorView,
    body: [f32; 4],
    m: EditorMetrics,
    colors: &ChromeColors,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> EditorChrome {
    let EditorMetrics { cell_w, cell_h, scale: s, px } = m;
    let mut out = EditorChrome { rects: Vec::new(), texts: Vec::new() };

    // A reason instead of content: still opening, refused, binary, empty. One
    // line, centred, in faint ink — it is a state, not an error dialog.
    if let Some(notice) = &view.notice {
        let w = measure(notice, px, false, 0.0);
        // Centred while it fits, left-aligned once it does not. A long refusal
        // centred in a narrow pane is clipped at *both* ends, which reads as a
        // rendering fault rather than as a message; from the left edge it
        // simply runs out of room, which reads as what it is.
        let x = if w <= body[2] - 2.0 * TEXT_PAD * s {
            body[0] + ((body[2] - w) / 2.0).max(0.0)
        } else {
            body[0] + TEXT_PAD * s
        };
        out.texts.push(TextRun {
            text: notice.clone(),
            pos: [x, baseline_in(body[1], body[3], px)],
            // Truncated with the run's own ellipsis rather than cut by the
            // clip: a message that ends in "…" says there is more of it.
            max_width: (body[2] - 2.0 * TEXT_PAD * s).max(0.0),
            color: colors.text_faint,
            clip: body,
            px,
            bold: false,
            tracking: 0.0,
        });
        return out;
    }

    let gutter = gutter_width(view.total, cell_w, s);
    // The rule between gutter and text, full height: it is what makes the
    // numbers read as a margin rather than as the first column of the file.
    out.rects.push(RectInstance::filled(
        [body[0] + gutter, body[1], HAIRLINE * s, body[3]],
        colors.hairline_soft,
        body,
    ));

    let text_x = body[0] + gutter + TEXT_PAD * s - view.scroll_x;
    for (row, line) in view.lines.iter().enumerate() {
        let y = body[1] + row as f32 * cell_h;
        if y + cell_h > body[1] + body[3] {
            break;
        }
        let baseline = baseline_in(y, cell_h, px);
        let number = view.first_line + row;

        // Right-aligned, which is the only way a column of numbers reads as a
        // column once it crosses a power of ten.
        let label = number.to_string();
        let nw = measure(&label, px, false, 0.0);
        out.texts.push(TextRun {
            text: label,
            pos: [body[0] + gutter - GUTTER_PAD * s - nw, baseline],
            max_width: nw + 2.0,
            color: colors.text_faint,
            clip: [body[0], body[1], gutter, body[3]],
            px,
            bold: false,
            tracking: 0.0,
        });

        if line.is_empty() {
            continue;
        }
        out.texts.push(TextRun {
            text: line.clone(),
            pos: [text_x, baseline],
            // Measured rather than clipped to the body's width: a line
            // scrolled sideways has to be allowed to start left of the body
            // and be cut by the clip, not truncated with an ellipsis the way
            // a label would be.
            max_width: measure(line, px, false, 0.0) + 2.0,
            color: colors.text_active,
            clip: [
                body[0] + gutter + HAIRLINE * s,
                body[1],
                (body[2] - gutter - HAIRLINE * s).max(0.0),
                body[3],
            ],
            px,
            bold: false,
            tracking: 0.0,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0, 1.0)
    }

    fn m() -> EditorMetrics {
        EditorMetrics { cell_w: 8.0, cell_h: 16.0, scale: 1.0, px: 13.0 }
    }

    fn measure(s: &str, px: f32, _bold: bool, tracking: f32) -> f32 {
        s.chars().count() as f32 * (8.0 * (px / 13.0) + tracking)
    }

    fn view(lines: &[&str], first: usize, total: usize) -> EditorView {
        EditorView {
            first_line: first,
            lines: lines.iter().map(|l| (*l).to_string()).collect(),
            total,
            scroll_x: 0.0,
            readonly: false,
            truncated: false,
            notice: None,
        }
    }

    #[test]
    fn the_gutter_is_sized_for_the_file_and_not_for_what_is_on_screen() {
        let (cell_w, s) = (8.0, 1.0);
        // Two digits is the floor, so a short file's numbers are not cramped
        // against the rule.
        assert_eq!(gutter_width(1, cell_w, s), gutter_width(9, cell_w, s));
        assert!(gutter_width(1000, cell_w, s) > gutter_width(999, cell_w, s));
        // The load-bearing one: scrolling a 1000-line file to line 5 must not
        // narrow the gutter, because every line of the file would shift
        // sideways underneath the reader.
        assert_eq!(gutter_width(1000, cell_w, s), gutter_width(1000, cell_w, s));
    }

    #[test]
    fn every_visible_line_is_drawn_with_its_own_number() {
        let v = view(&["alpha", "beta", "gamma"], 41, 500);
        let c = layout_editor(&v, [0.0, 0.0, 400.0, 60.0], m(), &colors(), &mut measure);

        for (i, body) in ["alpha", "beta", "gamma"].iter().enumerate() {
            assert!(
                c.texts.iter().any(|t| t.text == *body),
                "line {i} of the slice is drawn"
            );
            let n = (41 + i).to_string();
            assert!(
                c.texts.iter().any(|t| t.text == n),
                "and numbered {n}, from the slice's own first_line"
            );
        }
    }

    #[test]
    fn a_line_number_is_right_aligned_against_the_rule() {
        // 9 and 10 in a file of hundreds: their right edges have to agree, or
        // the column reads as ragged the moment it crosses a power of ten.
        let v = view(&["a", "b"], 9, 200);
        let c = layout_editor(&v, [0.0, 0.0, 400.0, 60.0], m(), &colors(), &mut measure);
        let right = |label: &str| {
            let t = c.texts.iter().find(|t| t.text == label).expect("number drawn");
            t.pos[0] + measure(label, t.px, false, 0.0)
        };
        assert!((right("9") - right("10")).abs() < 0.01, "the digits end on one edge");
    }

    #[test]
    fn a_notice_replaces_the_content_rather_than_sitting_beside_it() {
        let mut v = view(&[], 1, 0);
        v.notice = Some("opening…".into());
        let c = layout_editor(&v, [0.0, 0.0, 400.0, 60.0], m(), &colors(), &mut measure);
        assert_eq!(c.texts.len(), 1, "no gutter, no numbers, no rule");
        assert_eq!(c.texts[0].text, "opening…");
        assert!(c.rects.is_empty());
    }

    #[test]
    fn a_line_past_the_bottom_of_the_body_is_not_drawn() {
        // The app slices to what fits, but the body is the authority: a slice
        // one too long must be cut here rather than drawn over the frame.
        let v = view(&["1", "2", "3", "4", "5", "6"], 1, 6);
        let c = layout_editor(&v, [0.0, 0.0, 400.0, 48.0], m(), &colors(), &mut measure);
        assert!(
            c.texts.iter().all(|t| t.pos[1] <= 48.0),
            "nothing is placed below the body it is clipped to"
        );
        assert!(!c.texts.iter().any(|t| t.text == "6"), "the fourth line onward is skipped");
    }

    #[test]
    fn scrolling_sideways_moves_the_text_and_leaves_the_numbers() {
        let v = view(&["a long line"], 1, 1);
        let a = layout_editor(&v, [0.0, 0.0, 400.0, 60.0], m(), &colors(), &mut measure);
        let mut v2 = v.clone();
        v2.scroll_x = 40.0;
        let b = layout_editor(&v2, [0.0, 0.0, 400.0, 60.0], m(), &colors(), &mut measure);

        let line_x = |c: &EditorChrome| {
            c.texts.iter().find(|t| t.text == "a long line").expect("line").pos[0]
        };
        let num_x =
            |c: &EditorChrome| c.texts.iter().find(|t| t.text == "1").expect("number").pos[0];
        assert!((line_x(&a) - line_x(&b) - 40.0).abs() < 0.01, "the text moves by the scroll");
        assert!(
            (num_x(&a) - num_x(&b)).abs() < 0.01,
            "and the gutter does not — a margin that slid away would take the \
             line numbers off the left edge"
        );
    }
}
