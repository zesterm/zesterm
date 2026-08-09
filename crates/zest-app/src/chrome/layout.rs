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
}

// Logical-pixel constants, scaled at use. Named because the tests reason
// about them; not settings, because nobody should have to care.
const TAB_MIN: f32 = 120.0;
const TAB_MAX: f32 = 220.0;
const TAB_VPAD: f32 = 4.0;
const TAB_HPAD: f32 = 2.0;
const TEXT_PAD: f32 = 8.0;
const RADIUS: f32 = 6.0;
const CLOSE: f32 = 16.0;
const NEW_TAB: f32 = 24.0;
const PILL_HPAD: f32 = 5.0;
const HAIRLINE: f32 = 1.0;
const EDGE_PAD: f32 = 8.0;
const ROW_H: f32 = 44.0;
const ROW_HPAD: f32 = 6.0;
const HEADER_MIN: f32 = 28.0;
const LINE_GAP: f32 = 2.0;

pub fn layout(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
) -> ChromeLayout {
    match model.position {
        TabsPosition::Top => horizontal(model, colors, m, measure),
        TabsPosition::Left => vertical(model, colors, m, measure),
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
    measure: &mut dyn FnMut(&str) -> f32,
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
    // window drag like the rest of the empty strip.
    let reserve = model.traffic_inset.map_or(0.0, |t| t[0]) + EDGE_PAD * s;
    out.hit.push([0.0, 0.0, reserve, sh], HitRegion::Drag);

    let avail = (m.width - reserve - EDGE_PAD * s).max(0.0);
    let clip = [reserve, 0.0, avail, sh];

    let n = model.tabs.len();
    let tab_w = if n == 0 {
        0.0
    } else {
        (avail / n as f32).clamp(TAB_MIN * s, TAB_MAX * s)
    };
    let new_tab_w = NEW_TAB * s;
    let content_w = n as f32 * tab_w + new_tab_w;
    let max_scroll = (content_w - avail).max(0.0);
    out.strip_scroll = model.strip_scroll.clamp(0.0, max_scroll);

    for (i, tab) in model.tabs.iter().enumerate() {
        let x = reserve + i as f32 * tab_w - out.strip_scroll;
        let slot = [x, 0.0, tab_w, sh];
        let chip = [x + TAB_HPAD * s, TAB_VPAD * s, tab_w - 2.0 * TAB_HPAD * s, sh - 2.0 * TAB_VPAD * s];

        let active = i == model.active;
        let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
        if active {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_active_bg, clip));
        } else if hovered {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_hover_bg, clip));
        }
        if let Some(hit) = intersect(slot, clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        // Right-to-left within the tab: close button, then the origin pill,
        // and the title gets whatever is left.
        let mut right = x + tab_w - TEXT_PAD * s;

        if active || hovered {
            let close = [right - CLOSE * s, (sh - CLOSE * s) / 2.0, CLOSE * s, CLOSE * s];
            if let Some(hit) = intersect(close, clip) {
                out.hit.push(hit, HitRegion::TabClose(tab.addr));
            }
            let glyph_w = measure("×");
            out.texts.push(TextRun {
                text: "×".into(),
                pos: [close[0] + (close[2] - glyph_w) / 2.0, text_baseline(m, 0.0, sh)],
                max_width: close[2],
                color: colors.text_inactive,
                clip,
            });
            right = close[0] - TEXT_PAD * s / 2.0;
        }

        if let Some((label, warn)) = pill_label(tab) {
            let text_w = measure(&label);
            // The pill may take up to half the tab; past that the label
            // truncates rather than squeezing the title out entirely.
            let pill_w = (text_w + 2.0 * PILL_HPAD * s).min(tab_w * 0.5);
            let pill_h = (m.line_height + 2.0 * s).min(sh - 2.0 * TAB_VPAD * s);
            let pill = [right - pill_w, (sh - pill_h) / 2.0, pill_w, pill_h];
            let (bg, fg) = if warn {
                (colors.pill_warn_bg, colors.pill_warn_text)
            } else {
                (colors.pill_bg, colors.pill_text)
            };
            out.rects.push(RectInstance::rounded(pill, pill_h / 2.0, bg, clip));
            out.texts.push(TextRun {
                text: label,
                pos: [pill[0] + PILL_HPAD * s, text_baseline(m, 0.0, sh)],
                max_width: pill_w - 2.0 * PILL_HPAD * s,
                color: fg,
                clip,
            });
            right = pill[0] - TEXT_PAD * s / 2.0;
        }

        let text_x = x + TEXT_PAD * s;
        let title_color = match (active, model.focused, tab.connecting) {
            (_, _, true) => colors.text_faint,
            (true, true, _) => colors.text_active,
            _ => colors.text_inactive,
        };
        out.texts.push(TextRun {
            text: tab.title.clone(),
            pos: [text_x, text_baseline(m, 0.0, sh)],
            max_width: (right - text_x).max(0.0),
            color: title_color,
            clip,
        });
    }

    // The new-tab button trails the last tab and scrolls with the content.
    let nt_x = reserve + n as f32 * tab_w - out.strip_scroll;
    let nt = [nt_x + 2.0 * s, (sh - NEW_TAB * s) / 2.0, NEW_TAB * s, NEW_TAB * s];
    if model.hover == Some(HitRegion::NewTab) {
        out.rects.push(RectInstance::rounded(nt, RADIUS * s, colors.tab_hover_bg, clip));
    }
    if let Some(hit) = intersect(nt, clip) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    let plus_w = measure("+");
    out.texts.push(TextRun {
        text: "+".into(),
        pos: [nt[0] + (nt[2] - plus_w) / 2.0, text_baseline(m, 0.0, sh)],
        max_width: nt[2],
        color: colors.text_inactive,
        clip,
    });

    // Whatever the content does not cover is a drag handle, like any titlebar.
    let drag_from = (nt[0] + nt[2] + 2.0 * s).min(m.width);
    if drag_from < m.width {
        out.hit.push([drag_from, 0.0, m.width - drag_from, sh], HitRegion::Drag);
    }

    out
}

fn vertical(
    model: &ChromeModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str) -> f32,
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
    let header_h = model.traffic_inset.map_or(HEADER_MIN * s, |t| t[1].max(HEADER_MIN * s));
    out.hit.push([0.0, 0.0, sw, header_h], HitRegion::Drag);

    let rows_clip = [0.0, header_h, sw, (m.height - header_h).max(0.0)];
    let row_h = ROW_H * s;
    let n = model.tabs.len();
    let content_h = (n as f32 + 1.0) * row_h; // + the new-tab row
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    out.strip_scroll = model.strip_scroll.clamp(0.0, max_scroll);

    for (i, tab) in model.tabs.iter().enumerate() {
        let y = header_h + i as f32 * row_h - out.strip_scroll;
        let row = [0.0, y, sw, row_h];
        let chip = [ROW_HPAD * s, y + 2.0 * s, sw - 2.0 * ROW_HPAD * s, row_h - 4.0 * s];

        let active = i == model.active;
        let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
        if active {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_active_bg, rows_clip));
        } else if hovered {
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_hover_bg, rows_clip));
        }
        if let Some(hit) = intersect(row, rows_clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        let mut right = sw - (ROW_HPAD + TEXT_PAD) * s;
        if active || hovered {
            let close = [right - CLOSE * s, y + (row_h - CLOSE * s) / 2.0, CLOSE * s, CLOSE * s];
            if let Some(hit) = intersect(close, rows_clip) {
                out.hit.push(hit, HitRegion::TabClose(tab.addr));
            }
            let glyph_w = measure("×");
            out.texts.push(TextRun {
                text: "×".into(),
                pos: [
                    close[0] + (close[2] - glyph_w) / 2.0,
                    text_baseline(m, y, row_h),
                ],
                max_width: close[2],
                color: colors.text_inactive,
                clip: rows_clip,
            });
            right = close[0] - TEXT_PAD * s / 2.0;
        }

        let text_x = (ROW_HPAD + TEXT_PAD) * s;
        let title_color = match (active, model.focused, tab.connecting) {
            (_, _, true) => colors.text_faint,
            (true, true, _) => colors.text_active,
            _ => colors.text_inactive,
        };

        // The sidebar is the loud fleet view: every remote tab gets a second
        // line naming its machine, in words.
        if let Some((label, warn)) = pill_label(tab) {
            let block = 2.0 * m.line_height + LINE_GAP * s;
            let top = y + (row_h - block).max(0.0) / 2.0;
            out.texts.push(TextRun {
                text: tab.title.clone(),
                pos: [text_x, top + m.baseline],
                max_width: (right - text_x).max(0.0),
                color: title_color,
                clip: rows_clip,
            });
            out.texts.push(TextRun {
                text: label,
                pos: [text_x, top + m.line_height + LINE_GAP * s + m.baseline],
                max_width: (right - text_x).max(0.0),
                color: if warn { colors.pill_warn_text } else { colors.text_faint },
                clip: rows_clip,
            });
        } else {
            out.texts.push(TextRun {
                text: tab.title.clone(),
                pos: [text_x, text_baseline(m, y, row_h)],
                max_width: (right - text_x).max(0.0),
                color: title_color,
                clip: rows_clip,
            });
        }
    }

    // New-tab row.
    let y = header_h + n as f32 * row_h - out.strip_scroll;
    let row = [0.0, y, sw, row_h];
    if model.hover == Some(HitRegion::NewTab) {
        let chip = [ROW_HPAD * s, y + 2.0 * s, sw - 2.0 * ROW_HPAD * s, row_h - 4.0 * s];
        out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.tab_hover_bg, rows_clip));
    }
    if let Some(hit) = intersect(row, rows_clip) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    out.texts.push(TextRun {
        text: "+".into(),
        pos: [(ROW_HPAD + TEXT_PAD) * s, text_baseline(m, y, row_h)],
        max_width: sw,
        color: colors.text_inactive,
        clip: rows_clip,
    });

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
        TabModel {
            addr: addr(n),
            title: format!("tab {n}"),
            origin,
            presence,
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
        }
    }

    /// Eight pixels a character: enough for the tests to reason about
    /// truncation without a font.
    fn measure(s: &str) -> f32 {
        s.chars().count() as f32 * 8.0
    }

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
        assert_eq!(
            l.hit.hit(30.0, 40.0 + 22.0),
            Some(HitRegion::Tab(addr(1))),
            "the first row sits directly below the header"
        );
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
            assert!(
                l.texts.iter().any(|t| t.text.contains("alien") && t.text.contains("unreachable")),
                "{position:?} must spell out host and unreachability"
            );
        }
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
    fn an_empty_model_still_produces_chrome() {
        // Zero tabs happens transiently while the last tab closes; layout
        // must not panic and the strip must still exist.
        let m = metrics(800.0, 600.0, 1.0);
        let l = layout(&model(Vec::new(), TabsPosition::Top), &colors(), &m, &mut measure);
        assert!(!l.hit.is_empty());
        assert!(!l.rects.is_empty());
    }
}
