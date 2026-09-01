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
use zest_render_wgpu::{border_sides, LinearRgba, RectInstance};

use super::hit::{CaptionButton, ChromeHitMap, HitRegion, ResizeEdge};
use super::model::{
    AccentChoice, ChromeMetrics, ChromeModel, ConfirmChoices, LinkKind, TabKind, TabPresence,
};
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
    /// Same contract for the cwd chip's directory browser (#439).
    pub dir_picker_scroll: f32,
    /// The settings overlay's scroll, clamped — and possibly *adjusted*, when
    /// the model asked for the selection to be brought into view.
    pub settings_scroll: f32,
    /// The profiles editor's rows-pane scroll, same discipline. Its own
    /// field because the Settings tab can be open (inactive) underneath the
    /// Profiles screen, and two panes sharing one scroll would fight.
    pub profiles_scroll: f32,
    /// Slider tracks by row index, exactly as drawn — a click's fraction is
    /// computed against these, so pointer and pixels cannot disagree.
    pub settings_tracks: Vec<(usize, [f32; 4])>,
    /// The `+` button, exactly as drawn this pass — the launcher menu
    /// anchors to it, so the button and its menu come from one computation
    /// and cannot drift apart (the hit map's discipline, applied to
    /// anchoring).
    pub new_tab_rect: [f32; 4],
    /// Index into `rects`/`texts` where the overlay layer (picker, palette,
    /// settings) begins. The renderer draws base chrome's text before the
    /// overlay's panel, so text under a panel cannot bleed through it.
    pub overlay_rects_at: usize,
    pub overlay_texts_at: usize,
}

// Logical-pixel constants, scaled at use. Named because the tests reason
// about them; not settings, because nobody should have to care. The values
// are the design handoff's (docs/design/client-ui/README.md) — change them
// there first or not at all.
const TAB_H: f32 = 34.0;
/// Chips stop shrinking here; past it the strip scrolls. Below 104 the
/// label degrades to a single letter (design §1).
const TAB_MIN: f32 = 104.0;
/// Hard ceiling on any chip — in practice only a content-sized app tab with
/// a long label can reach for it, since session chips never grow past basis.
const TAB_MAX: f32 = 232.0;
/// The preferred chip width (the mock's `flex: 0 1 168px`): chips shrink
/// from here, they never grow past it.
const TAB_BASIS: f32 = 168.0;
const TAB_GAP: f32 = 3.0;
const TAB_PAD: f32 = 11.0;
const TAB_INNER_GAP: f32 = 9.0;
const TAB_RADIUS: f32 = 9.0;
const ACCENT_RULE: f32 = 2.0;
const DOT: f32 = 6.0;
/// The chip's rounded glyph tile: the profile's icon in the tab's colour on
/// a 12%-alpha wash of it (design §1). The icon itself is a placeholder dot
/// until profiles land — position and size are what this pins.
const TILE: f32 = 18.0;
const TILE_RADIUS: f32 = 5.0;
const TEXT_PAD: f32 = 8.0;
const RADIUS: f32 = 6.0;
const CLOSE: f32 = 16.0;
/// The attention badge on a chip's glyph tile — small enough to read as a
/// mark on the icon rather than a second icon beside it.
const BADGE: f32 = 6.0;
const NEW_TAB_W: f32 = 28.0;
const NEW_TAB_H: f32 = 30.0;
const PILL_H: f32 = 26.0;
const PILL_RADIUS: f32 = 7.0;
const PILL_PAD: f32 = 9.0;
const PILL_GAP: f32 = 6.0;
const PILL_HPAD: f32 = 5.0;
pub(super) const HAIRLINE: f32 = 1.0;
const EDGE_PAD: f32 = 8.0;
const BAR_PAD: f32 = 12.0;
const TRAFFIC_PAD: f32 = 14.0;
/// Caption button width. Windows' own metric, so the Snap Layouts flyout —
/// which points at whatever rect answers `HTMAXBUTTON` — lands where the user
/// expects rather than beside it.
const CAPTION_W: f32 = 46.0;
/// The minimise bar and the maximise square, both this wide.
const CAPTION_GLYPH: f32 = 10.0;
/// Type size for the close `×`.
const CAPTION_X: f32 = 15.0;
/// How wide a band along the window edge starts a resize. Windows' own frame
/// is ~8 physical px at 100%; 5 logical scales with DPI and stays inside the
/// default `window.padding` of 8, so it never eats a column of grid text.
const RESIZE_BAND: f32 = 5.0;
const ROW_HPAD: f32 = 8.0;
// Sidebar (design screen 2).
const SEARCH_PAD: f32 = 10.0;
const SEARCH_H: f32 = 30.0;
const GROUP_HEADER_H: f32 = 26.0;
const GROUP_GAP: f32 = 14.0;
const SIDE_ROW_H: f32 = 44.0;
/// App-tab rows in the sidebar (§11/§12). Shorter than a session row
/// because they carry no cwd line — the only thing the second line was for.
const APP_ROW_H: f32 = 32.0;
const FOOTER_H: f32 = 42.0;
/// The vertical layout's full-width header row (design §2). Public because
/// `insets_at` must reserve it above the grid, exactly as it reserves the
/// strip: a bar the grid does not know about paints over row 0.
pub const HEADER_H: f32 = 46.0;
const SLIM_PAD: f32 = 14.0;

// Split panes (design screen 5). Public because the app's viewport math and
// the chrome's frame drawing must be the same numbers or the border misses
// the grid it frames.
pub const PANE_MARGIN: f32 = 8.0;
pub const PANE_HEADER: f32 = 28.0;
pub const PANE_RADIUS: f32 = 10.0;

/// The `n` pane frames of a split tab, left to right inside the grid area:
/// equal columns, a margin around each (#436). `n == 0` is treated as one
/// frame rather than dividing by zero — a caller a frame behind a pane close
/// still gets a rectangle to draw in.
#[must_use]
pub fn pane_frames(area: [f32; 4], s: f32, n: usize) -> Vec<[f32; 4]> {
    let n = n.max(1);
    // The margin gives way before the frames do: past the width where n
    // margins no longer fit, a fixed margin would march the later frames off
    // the right edge — hit regions nobody can reach, behind chrome nobody
    // can see. Bounded by the share of the width, every frame stays inside.
    let m = (PANE_MARGIN * s).min(area[2] / (2.0 * n as f32)).max(0.0);
    let w = ((area[2] - 2.0 * m * n as f32) / n as f32).max(0.0);
    let h = (area[3] - 2.0 * m).max(0.0);
    (0..n).map(|i| [area[0] + m + i as f32 * (w + 2.0 * m), area[1] + m, w, h]).collect()
}

/// Where a pane's grid actually lives: inside the frame, below the header,
/// and inside `window.padding` on the left and right.
///
/// That horizontal padding is the block rail's room. The rail is drawn in the
/// grid layer *outside* the grid rect — chrome painted inside it shaves column
/// 0 off every output row — so it needs free pixels beside the cells or it is
/// silently not drawn. A pane used to inset by a hairline only, and since a
/// pane's grid is sized `floor(body_w / cell_w)` the letterbox then returned
/// the body rect exactly: a gutter of 0.0, in every pane, always (#460).
///
/// `padding` is the same user-facing setting the unsplit path gives its own
/// gutter, so setting it to zero drops the rail in both layouts together
/// rather than in one. Floored for the reason `Insets::resolved` states: this
/// becomes the grid's origin, and a fractional origin resamples every glyph
/// between texels through the `Nearest` atlas sampler.
#[must_use]
pub fn pane_body(frame: [f32; 4], s: f32, padding: u32) -> [f32; 4] {
    let b = HAIRLINE * s;
    let p = b + (padding as f32 * s).floor();
    [
        frame[0] + p,
        frame[1] + PANE_HEADER * s,
        (frame[2] - 2.0 * p).max(0.0),
        (frame[3] - PANE_HEADER * s - b).max(0.0),
    ]
}

/// Where each pane's grid is drawn inside `area`, left to right — one
/// rectangle per entry in `grids`, which gives each pane's granted
/// `(cols, rows)`.
///
/// The one copy of this arithmetic. It had three — the pointer's rectangle,
/// the resize, and the render pass — and the comment tying them together said
/// only that the focused one "must come out equal to `focused_view_rect`".
///
/// A single grid is the unsplit window: `area` letterboxed, with no frame and
/// no pane padding, so an unsplit tab renders byte-identically to what it did
/// before panes existed (the #44 pixel assertions and #215 both depend on it).
#[must_use]
pub fn pane_grid_rects(
    area: [f32; 4],
    s: f32,
    padding: u32,
    grids: &[(usize, usize)],
    m: zest_font::CellMetrics,
) -> Vec<[f32; 4]> {
    let lb = |rect: [f32; 4], (cols, rows): (usize, usize)| {
        super::insets::letterbox(rect, cols, rows, m)
    };
    match grids {
        [] => Vec::new(),
        [one] => vec![lb(area, *one)],
        many => pane_frames(area, s, many.len())
            .into_iter()
            .zip(many)
            .map(|(f, g)| lb(pane_body(f, s, padding), *g))
            .collect(),
    }
}

/// Free pixels between a pane's border and the grid inside it — the block
/// rail's room, and the pane's answer to what `window.padding` gives the
/// unsplit path.
///
/// `grid` is the *letterboxed* rect, not [`pane_body`]'s: under size
/// arbitration (#215) a grid narrower than its pane sits centered in it, and
/// that slack is room for the rail too. One function because the two numbers
/// have to agree — the padding [`pane_body`] takes out and the space the
/// renderer is told about — and a second copy is how one of them drifts.
#[must_use]
pub fn pane_gutter(frame: [f32; 4], grid: [f32; 4], s: f32) -> f32 {
    (grid[0] - frame[0] - HAIRLINE * s).max(0.0)
}

// The design's type scale, logical px.
const UI_BODY: f32 = 12.5;
const UI_SMALL: f32 = 11.0;
const UI_CHORD: f32 = 10.0;

/// The glyph an app tab wears in both positions — ⚙ for Settings, ▤ for
/// Profiles. One function so the chip and the sidebar row cannot drift; both
/// are BMP, so ordinary font fallback finds them where a PUA icon would need
/// a Nerd Font installed.
#[must_use]
fn app_tab_glyph(kind: TabKind) -> &'static str {
    match kind {
        TabKind::Profiles => "\u{25a4}",
        _ => "\u{2699}",
    }
}
const UI_STATUS: f32 = 10.5;

/// Baseline that vertically centres a run of `px`-sized text in a band.
/// 0.72·px approximates the ascent above baseline for the faces we ship;
/// exact per-face metrics would need the font here, and being one pixel
/// off is invisible while being *inconsistent* is not — every band uses
/// this one rule.
pub(super) fn baseline_in(band_y: f32, band_h: f32, px: f32) -> f32 {
    band_y + (band_h + px * 0.72) / 2.0
}

/// The host-accent cycle: slot 0 (the local machine) is `success`, then
/// `info`, `magenta`, `warn` — the design's studio/crate/forge assignment
/// generalized. Wraps rather than running out.
pub(super) fn host_accent(colors: &ChromeColors, slot: usize) -> LinearRgba {
    [colors.success, colors.info, colors.magenta, colors.warn][slot % 4]
}

/// A chip's accent choice, resolved to ink (design §12).
///
/// The profile row is the theme's accent-picker order — `accent` first, then
/// the state colours — because the theme carries no separate accents list;
/// these six are what its swatches show. Wraps rather than running out, like
/// the host cycle: a hand-edited `tab_color = 250` still draws something.
pub(super) fn accent_color(colors: &ChromeColors, choice: AccentChoice) -> LinearRgba {
    match choice {
        AccentChoice::Profile(i) => [
            colors.accent,
            colors.success,
            colors.warn,
            colors.danger,
            colors.info,
            colors.magenta,
        ][usize::from(i) % 6],
        AccentChoice::Host(slot) => host_accent(colors, slot),
    }
}

/// A little status dot, as the SDF pipeline draws circles: a square rect
/// with radius d/2.
/// How a [`ring`] is drawn: turning, or standing still at a fraction.
///
/// An enum rather than "a fraction of 1.0 means spin", which is what this was
/// first: a determinate bar reaching 100% is exactly `1.0`, so the one state
/// that means *finished* rendered as the one that means *still going*. The two
/// are different pictures and inferring one from the other's edge value is how
/// they came to be the same one.
#[derive(Debug, Clone, Copy)]
pub(super) enum RingStyle {
    /// Busy, no idea how far: one small bite orbiting on the clock.
    Spin(f32),
    /// `0.0..=1.0` of the ring left whole. `1.0` is a closed ring, and closed
    /// is what "done" looks like.
    Arc(f32),
}

/// A spinning ring, or an arc of one.
///
/// An SDF box cannot draw an arc, so this is a ring with a *bite* taken out of
/// it in the colour of whatever is behind — which reads exactly the same and
/// costs two rects.
///
/// **`bg` is not optional and cannot be defaulted.** The bite has to match
/// what is underneath it, and a tab chip's background differs between its
/// active fill, the strip, and a hover fill — which is exactly why the block
/// header's version could hard-code one and this one cannot. Passing the
/// wrong one draws a coloured notch rather than a gap.
pub(super) fn ring(
    rects: &mut Vec<RectInstance>,
    rect: [f32; 4],
    ink: LinearRgba,
    bg: LinearRgba,
    style: RingStyle,
    clip: [f32; 4],
) {
    let d = rect[2].min(rect[3]);
    let r = d / 2.0;
    let (cx, cy) = (rect[0] + r, rect[1] + r);
    rects.push(RectInstance {
        radii: [d / 2.0; 4],
        border: ink,
        border_width: 1.5 * (d / 8.0).max(1.0),
        ..RectInstance::filled([rect[0], rect[1], d, d], LinearRgba::TRANSPARENT, clip)
    });
    let bite = (d * 0.4).max(2.0);
    // Each gap is a rounded square sitting *on* the stroke, so a sweep needs
    // several: one wide rect would be a chord, and past a few degrees a chord
    // cuts across the ring rather than along it.
    let mut gap = |t: f32| {
        let angle = (t - 0.25) * core::f32::consts::TAU;
        rects.push(RectInstance::rounded(
            [
                cx + angle.cos() * r - bite / 2.0,
                cy + angle.sin() * r - bite / 2.0,
                bite,
                bite,
            ],
            bite / 2.0,
            bg,
            clip,
        ));
    };
    match style {
        RingStyle::Spin(phase) => gap(phase),
        RingStyle::Arc(fraction) => {
            let missing = (1.0 - fraction).clamp(0.0, 1.0);
            // A closed ring: nothing to erase, and nothing that moves.
            if missing <= f32::EPSILON {
                return;
            }
            let steps = ((missing * 24.0).ceil() as usize).max(1);
            for i in 0..steps {
                // Sweeping backwards from the top, so a bar fills clockwise
                // the way every other progress indicator does.
                gap(fraction + missing * (i as f32 / steps as f32));
            }
        }
    }
}

/// A spinning ring drawn as the arc that is lit, rather than as a full ring
/// with a bite erased out of it.
///
/// [`ring`] needs an opaque `bg` because its gap is a rect painted in whatever
/// is behind — which works on a tab chip, and does not work anywhere the thing
/// behind is the wallpaper. A block header has no fill since #465, so its
/// spinner cannot erase anything: the gap here is an *absence*, so there is
/// nothing to match and the picture is right over a background image, a
/// translucent window, or a TUI's own colours.
///
/// Segments rather than one swept shape for [`ring`]'s reason: an SDF box
/// cannot draw an arc, and one wide rect would be a chord — past a few degrees
/// a chord cuts across the ring instead of running along it.
pub(super) fn arc(
    rects: &mut Vec<RectInstance>,
    rect: [f32; 4],
    ink: LinearRgba,
    phase: f32,
    clip: [f32; 4],
) {
    let d = rect[2].min(rect[3]);
    let r = d / 2.0;
    let (cx, cy) = (rect[0] + r, rect[1] + r);
    let seg = (d * 0.28).max(1.5);
    // Twelve reads as a circle and leaves a gap wide enough to see turning;
    // fewer reads as a dotted ring, more closes the gap up.
    const STEPS: usize = 12;
    /// How much of the turn is dark, in steps. Two is the bite `ring` takes.
    const GAP: usize = 2;
    for i in GAP..STEPS {
        let t = phase + i as f32 / STEPS as f32;
        let angle = (t - 0.25) * core::f32::consts::TAU;
        rects.push(RectInstance::rounded(
            [cx + angle.cos() * r - seg / 2.0, cy + angle.sin() * r - seg / 2.0, seg, seg],
            seg / 2.0,
            ink,
            clip,
        ));
    }
}

fn dot(rects: &mut Vec<RectInstance>, cx: f32, cy: f32, d: f32, color: LinearRgba, clip: [f32; 4]) {
    rects.push(RectInstance::rounded([cx - d / 2.0, cy - d / 2.0, d, d], d / 2.0, color, clip));
}

/// The colour at a fraction of its own alpha. Colours here are premultiplied,
/// so scaling every channel is the correct alpha multiply — the glyph tile's
/// 12% wash is its ink through this.
pub(crate) fn washed(c: LinearRgba, f: f32) -> LinearRgba {
    LinearRgba([c.0[0] * f, c.0[1] * f, c.0[2] * f, c.0[3] * f])
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
    if let Some(panes) = &model.panes {
        panes_overlay(panes, model.grid_area, colors, m, measure, &mut out);
    }
    let mut settings_menu_anchor = None;
    if let Some(settings) = &model.settings {
        // The Settings tab's content — window content like a screen, not an
        // overlay: it replaces the grid area while that tab is active. Its
        // open dropdown is the exception — an anchor comes back and the
        // menu draws in the overlay layer below (#182).
        settings_menu_anchor = super::settings_screen::settings_screen(
            settings,
            model.grid_area,
            colors,
            m,
            measure,
            &mut out,
        );
    }
    let mut screen_menu_anchor = None;
    if let Some(screen) = &model.screen {
        // Over the grid (and over the settings tab's content — Esc returns),
        // under the modals: a screen is window content, not an overlay, so
        // the picker can still open above it. The profiles editor's open
        // dropdown comes back as an anchor and draws below, like the
        // settings tab's (#182).
        screen_menu_anchor = super::screens::screen_overlay(
            screen,
            model.grid_area,
            colors,
            model.hover,
            m.scale,
            measure,
            &mut out,
        );
    }
    if let Some(notice) = &model.notice {
        // After the screens, before the modals: an approval prompt must
        // survive the fleet screen being open — the picker is where the
        // attach that is now pending usually started — while the modals
        // still rank above it, like everything else in the base layer.
        notice_bar(notice, model.grid_area, colors, m, measure, &mut out);
    }
    // Everything below is the overlay layer; everything above must have its
    // text drawn before an overlay's panel covers it.
    out.overlay_rects_at = out.rects.len();
    out.overlay_texts_at = out.texts.len();
    // The dropdowns first: they float over their screen's content, and the
    // modals (picker, palette) still open above them.
    if let (Some(settings), Some(anchor)) = (&model.settings, settings_menu_anchor) {
        if let Some(menu) = &settings.menu {
            super::settings_screen::dropdown_menu(
                menu,
                anchor,
                model.grid_area,
                colors,
                m.scale,
                measure,
                &mut out,
            );
        }
    }
    if let (Some(super::model::ScreenModel::Profiles(p)), Some(anchor)) =
        (&model.screen, screen_menu_anchor)
    {
        if let Some(menu) = &p.menu {
            super::settings_screen::dropdown_menu(
                menu,
                anchor,
                model.grid_area,
                colors,
                m.scale,
                measure,
                &mut out,
            );
        }
    }
    if let Some(picker) = &model.picker {
        // Appended last on purpose: last drawn is topmost, and last pushed
        // wins the hit lookup — the same fact, stated once.
        picker_overlay(picker, colors, m, measure, &mut out);
    }
    // The palette after the picker: it is also how a long-list *value* is
    // chosen from the settings tab, and that list opens on top of the
    // content it was opened from.
    if let Some(palette) = &model.palette {
        palette_overlay(palette, colors, m, measure, &mut out);
    }
    if let Some(picker) = &model.dir_picker {
        dir_picker_overlay(picker, colors, m, measure, &mut out);
    }
    if let Some(open_file) = &model.open_file {
        open_file_overlay(open_file, colors, m, measure, &mut out);
    }
    if let Some(launcher) = &model.launcher {
        // Anchored to the `+` the strip pass just recorded; exclusive with
        // the other overlays by the app's rule, so its place in this list
        // carries no ranking.
        launcher_overlay(launcher, model.hover, colors, m, measure, &mut out);
    }
    if let Some(menu) = &model.block_menu {
        // Clamped inside the grid area rather than the window: its anchor is a
        // block header, which lives there. Exclusive with the other overlays
        // by the app's rule, so its place in this list carries no ranking.
        super::block_menu::block_menu_overlay(
            menu,
            model.hover,
            model.grid_area,
            colors,
            m,
            measure,
            &mut out,
        );
    }
    if let Some(confirm) = &model.confirm_close {
        // Above the other overlays and below the approval modal. It opened
        // because the user pressed ⌘W, so it may own the window; the approval
        // opens on the network's schedule and outranks everything.
        confirm_close_overlay(confirm, model.hover, colors, m, measure, &mut out);
    }
    if let Some(approval) = &model.approval {
        // Above every other overlay, deliberately: unlike them it opens on
        // the *network's* schedule, not the user's, so it cannot rely on the
        // app's one-overlay-at-a-time rule — and its text is a security
        // decision nothing may cover.
        approval_overlay(approval, model.hover, colors, m, measure, &mut out);
    }
    // Dead last, after the modals, and that ordering is the feature: lookups
    // walk the map backwards, so the window's own edge outranks a palette
    // scrim. A window you cannot resize while an overlay is open would be a
    // strange thing to ship, and it is exactly what pushing these earlier
    // would produce.
    resize_edges(model, m, &mut out);
    out
}

/// The window's resize bands, when we own the frame.
///
/// A borderless window has no non-client area left, so `DefWindowProc` never
/// answers `HTLEFT`/`HTTOP` and the edges simply stop working — silently,
/// while maximise and snap keep going, which is what makes it easy to ship
/// broken. These bands are the replacement; the app turns each into
/// `Window::drag_resize_window`.
fn resize_edges(model: &ChromeModel, m: &ChromeMetrics, out: &mut ChromeLayout) {
    if !model.controls.resizable_edges {
        return;
    }
    let b = RESIZE_BAND * m.scale;
    let (w, h) = (m.width, m.height);
    // Edges first, then corners, so a corner wins where they overlap — a
    // 5px band means the corner is otherwise unhittable in the 5×5 square
    // where both apply, which is precisely where people aim for it.
    for (rect, edge) in [
        ([0.0, 0.0, w, b], ResizeEdge::N),
        ([0.0, h - b, w, b], ResizeEdge::S),
        ([0.0, 0.0, b, h], ResizeEdge::W),
        ([w - b, 0.0, b, h], ResizeEdge::E),
        ([0.0, 0.0, b * 2.0, b * 2.0], ResizeEdge::Nw),
        ([w - b * 2.0, 0.0, b * 2.0, b * 2.0], ResizeEdge::Ne),
        ([0.0, h - b * 2.0, b * 2.0, b * 2.0], ResizeEdge::Sw),
        ([w - b * 2.0, h - b * 2.0, b * 2.0, b * 2.0], ResizeEdge::Se),
    ] {
        out.hit.push(rect, HitRegion::Resize(edge));
    }
}

/// Lay the caption cluster into the right edge of `bar` (`[x, y, w, h]`),
/// returning the x that the rest of the bar may use.
///
/// Called from both the horizontal strip and the sidebar's slim bar, so Close
/// cannot end up in two different places depending on the tab position.
fn caption_cluster(
    out: &mut ChromeLayout,
    colors: &ChromeColors,
    model: &ChromeModel,
    bar: [f32; 4],
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> f32 {
    let right = bar[0] + bar[2];
    if !model.controls.drawn_caption {
        return right;
    }
    let w = CAPTION_W * s;
    let (y, h) = (bar[1], bar[3]);
    let clip = [bar[0], y, bar[2], h];
    // Right to left: close is outermost, which is where every Windows window
    // has put it and therefore where the pointer is already going.
    for (i, which) in
        [CaptionButton::Close, CaptionButton::Maximize, CaptionButton::Minimize].iter().enumerate()
    {
        let x = right - w * (i as f32 + 1.0);
        let rect = [x, y, w, h];
        let hovered = model.hover == Some(HitRegion::CaptionButton(*which));
        if hovered {
            // Red for close is the Windows convention, and worth following
            // exactly: it is the one button whose mis-click costs a session.
            let fill =
                if *which == CaptionButton::Close { colors.danger } else { colors.tab_hover_bg };
            out.rects.push(RectInstance::filled(rect, fill, clip));
        }
        let fg = if hovered && *which == CaptionButton::Close {
            colors.text_active
        } else {
            colors.text_faint
        };
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let g = CAPTION_GLYPH * s;
        // Glyphs from primitives rather than from `Segoe MDL2 Assets`: those
        // codepoints are Private Use Area, and PUA structurally cannot be
        // reached by script-based font fallback — the trap the Nerd Font work
        // already paid for once. A hairline square needs no font at all.
        let outline = |r: [f32; 4]| RectInstance {
            border: fg,
            border_width: HAIRLINE * s,
            ..RectInstance::filled(r, LinearRgba::TRANSPARENT, clip)
        };
        match which {
            CaptionButton::Minimize => {
                out.rects.push(RectInstance::filled(
                    [cx - g / 2.0, cy.round(), g, HAIRLINE * s],
                    fg,
                    clip,
                ));
            }
            CaptionButton::Maximize => {
                let square = |o: f32| [cx - g / 2.0 + o, cy - g / 2.0 - o, g, g];
                if model.controls.maximized {
                    // Restore is two offset outlines, the way Windows draws it.
                    out.rects.push(outline(square(2.0 * s)));
                }
                out.rects.push(outline(square(0.0)));
            }
            CaptionButton::Close => {
                // The same `×` the tab close button uses: Latin-1, present in
                // every face we could plausibly be shaping with.
                let px = CAPTION_X * s;
                let glyph_w = measure("\u{d7}", px, false, 0.0);
                out.texts.push(TextRun {
                    text: "\u{d7}".into(),
                    pos: [cx - glyph_w / 2.0, baseline_in(y, h, px)],
                    max_width: w,
                    color: fg,
                    clip,
                    px,
                    bold: false,
                    tracking: 0.0,
                });
            }
        }
        out.hit.push(rect, HitRegion::CaptionButton(*which));
    }
    right - w * 3.0
}

/// The split tab's frames and headers (design screen 5): focused pane gets
/// the accent border and the word "focused"; the other, hairline and dim.
fn panes_overlay(
    panes: &[super::model::PaneModel],
    area: [f32; 4],
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    use super::model::PaneKind;
    let s = m.scale;
    let frames = pane_frames(area, s, panes.len());
    for (i, (pane, frame)) in panes.iter().zip(frames).enumerate() {
        let header_h = PANE_HEADER * s;
        // Header band first, then the frame's border over everything, so the
        // rounded corners stay crisp where the band meets them.
        let header = [frame[0], frame[1], frame[2], header_h];
        out.rects.push(RectInstance {
            radii: [PANE_RADIUS * s, PANE_RADIUS * s, 0.0, 0.0],
            ..RectInstance::filled(
                header,
                if pane.focused { colors.panel_bg } else { colors.block_header_bg },
                area,
            )
        });
        out.rects.push(RectInstance::filled(
            [frame[0], frame[1] + header_h - HAIRLINE * s, frame[2], HAIRLINE * s],
            colors.line,
            area,
        ));
        out.rects.push(RectInstance {
            radii: [PANE_RADIUS * s; 4],
            border: if pane.focused { colors.accent } else { colors.line },
            border_width: HAIRLINE * s,
            ..RectInstance::filled(frame, LinearRgba::TRANSPARENT, area)
        });

        // The unfocused pane is one click from the keyboard, anywhere on it;
        // the focused pane's body stays the grid's, so only its header
        // answers as chrome.
        if pane.focused {
            out.hit.push(header, HitRegion::Pane(i));
        } else {
            out.hit.push(frame, HitRegion::Pane(i));
        }

        // A file pane's body is chrome all the way down — there is no grid
        // beneath it — so it draws here and claims the wheel for itself,
        // focused or not. Pushed *after* the `Pane` region so it wins inside
        // the body while the header still moves the keyboard.
        if let PaneKind::Editor(view) = &pane.kind {
            let body = pane_body(frame, s, m.padding);
            let ed = super::editor::layout_editor(
                view,
                body,
                super::editor::EditorMetrics {
                    cell_w: m.cell_w,
                    cell_h: m.line_height,
                    scale: s,
                    // The grid's own size: a file beside a terminal should be
                    // the same text at the same scale, not a UI label about it.
                    px: m.font_px,
                },
                colors,
                measure,
            );
            out.rects.extend(ed.rects);
            out.texts.extend(ed.texts);
            out.hit.push(body, HitRegion::EditorBody(i));
        }

        let mut x = frame[0] + 10.0 * s;
        dot(
            &mut out.rects,
            x + 2.5 * s,
            frame[1] + header_h / 2.0,
            5.0 * s,
            host_accent(colors, pane.accent),
            area,
        );
        x += 13.0 * s;
        let name_px = UI_SMALL * s;
        let nw = measure(&pane.host, name_px, false, 0.0);
        out.texts.push(TextRun {
            text: pane.host.clone(),
            pos: [x, baseline_in(frame[1], header_h, name_px)],
            max_width: nw + 2.0,
            color: if pane.focused { colors.text_active } else { colors.text_inactive },
            clip: area,
            px: name_px,
            bold: false,
            tracking: 0.0,
        });
        x += nw + 8.0 * s;
        let mut right_edge = frame[0] + frame[2] - 10.0 * s;
        if pane.focused {
            let fw = measure("focused", UI_CHORD * s, false, 0.0);
            out.texts.push(TextRun {
                text: "focused".into(),
                pos: [right_edge - fw, baseline_in(frame[1], header_h, UI_CHORD * s)],
                max_width: fw + 2.0,
                color: colors.accent,
                clip: area,
                px: UI_CHORD * s,
                bold: false,
                tracking: 0.0,
            });
            right_edge -= fw + 8.0 * s;
        }
        out.texts.push(TextRun {
            text: pane.sub.clone(),
            pos: [x, baseline_in(frame[1], header_h, UI_STATUS * s)],
            max_width: (right_edge - x).max(0.0),
            color: colors.text_faint,
            clip: area,
            px: UI_STATUS * s,
            bold: false,
            tracking: 0.0,
        });
    }
}

// The window-level notice bar (#190), logical px.
const NOTICE_H: f32 = 30.0;
const NOTICE_PAD: f32 = 14.0;
const NOTICE_TOP: f32 = 8.0;

/// A window-level notice, pinned centered to the top of the grid area — the
/// pairing approval prompt is the tenant today.
///
/// Deliberately **not** a hit target: there is nothing to click, and a region
/// here would steal the terminal's own top rows from selection. It rides the
/// grid area rather than the strip because a six-digit code has to be
/// readable, and a 34px chip cannot carry a sentence.
// The pairing approval modal (ROADMAP M4), logical px.
const APPROVAL_W: f32 = 460.0;
const APPROVAL_H: f32 = 190.0;
const APPROVAL_PAD: f32 = 20.0;
const APPROVAL_RADIUS: f32 = 12.0;
const APPROVAL_BTN_W: f32 = 96.0;
const APPROVAL_BTN_H: f32 = 30.0;
const APPROVAL_BTN_GAP: f32 = 10.0;
/// The code's type size — the one thing on the panel a person actually
/// compares, so it dwarfs everything else on it.
const APPROVAL_CODE_PX: f32 = 28.0;

/// The pairing approval modal: a device is asking, a person answers.
///
/// Drawn after every other overlay (see the call site) with a dimming scrim
/// that swallows and does not dismiss — the buttons and Esc are the only
/// exits, because "clicked it away by accident" must not be a state a
/// security prompt can reach.
fn approval_overlay(
    approval: &super::model::ApprovalModel,
    hover: Option<HitRegion>,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::ApprovalPanel);

    let w = (APPROVAL_W * s).min((m.width - 2.0 * EDGE_PAD * s).max(0.0));
    let h = APPROVAL_H * s;
    let x = (m.width - w) / 2.0;
    // The picker's band, roughly: high enough to read as a prompt, not a
    // sheet growing out of the grid.
    let y = ((m.height - h) * 0.32).max(0.0);
    let panel = [x, y, w, h];
    out.rects.push(RectInstance {
        radii: [APPROVAL_RADIUS * s; 4],
        border: colors.line,
        border_width: HAIRLINE * s,
        shadow_blur: 20.0 * s,
        shadow_alpha: colors.shadow_alpha,
        ..RectInstance::filled(panel, colors.panel_bg, no_clip)
    });
    out.hit.push(panel, HitRegion::ApprovalPanel);

    let pad = APPROVAL_PAD * s;
    // The question, with the device's own claim of a name and where it is
    // dialling from — the person's entire decision input, which is why the
    // label rides inside the signed transcript.
    let title = format!("Allow {} ({}) to attach?", approval.label, approval.remote);
    let title_px = UI_BODY * s;
    out.texts.push(TextRun {
        text: title,
        pos: [x + pad, y + pad + title_px * 0.8],
        max_width: w - 2.0 * pad,
        color: colors.text_active,
        clip: panel,
        px: title_px,
        bold: true,
        tracking: 0.0,
    });

    // The code, huge and centered: the person compares it digit by digit
    // with the asking device's screen.
    let code_px = APPROVAL_CODE_PX * s;
    let code_w = measure(&approval.code, code_px, true, 0.0);
    out.texts.push(TextRun {
        text: approval.code.clone(),
        pos: [x + (w - code_w) / 2.0, y + 62.0 * s + code_px * 0.72],
        max_width: w - 2.0 * pad,
        color: colors.accent,
        clip: panel,
        px: code_px,
        bold: true,
        tracking: 2.0 * s,
    });
    let sub = format!("compare this code on the asking device \u{b7} {}", approval.expires);
    let sub_px = UI_STATUS * s;
    let sub_w = measure(&sub, sub_px, false, 0.0);
    out.texts.push(TextRun {
        text: sub,
        pos: [x + (w - sub_w).max(0.0) / 2.0, y + 104.0 * s + sub_px * 0.72],
        max_width: w - 2.0 * pad,
        color: colors.text_faint,
        clip: panel,
        px: sub_px,
        bold: false,
        tracking: 0.0,
    });

    // Deny, then Approve at the far right — the affirmative in the corner
    // position every dialog on every platform has taught the hand.
    let btn_h = APPROVAL_BTN_H * s;
    let btn_w = APPROVAL_BTN_W * s;
    let by = y + h - pad - btn_h;
    for (i, (label, region, ink)) in [
        ("Deny", HitRegion::ApprovalDeny, colors.danger),
        ("Approve", HitRegion::ApprovalApprove, colors.accent),
    ]
    .into_iter()
    .enumerate()
    {
        let bx = x + w - pad - btn_w - (1 - i) as f32 * (btn_w + APPROVAL_BTN_GAP * s);
        let rect = [bx, by, btn_w, btn_h];
        let hovered = hover == Some(region);
        out.rects.push(RectInstance {
            radii: [7.0 * s; 4],
            border: ink,
            border_width: HAIRLINE * s,
            ..RectInstance::filled(
                rect,
                if hovered { colors.tab_hover_bg } else { colors.panel_bg },
                panel,
            )
        });
        let label_px = UI_BODY * s;
        let lw = measure(label, label_px, false, 0.0);
        out.texts.push(TextRun {
            text: label.into(),
            pos: [bx + (btn_w - lw) / 2.0, baseline_in(by, btn_h, label_px)],
            max_width: btn_w,
            color: ink,
            clip: panel,
            px: label_px,
            bold: false,
            tracking: 0.0,
        });
        out.hit.push(rect, region);
    }
}

// The close-confirm (#381), logical px. Shorter than the approval panel: it
// carries a question and a sentence, not a code to compare digit by digit.
const CONFIRM_W: f32 = 470.0;
const CONFIRM_H: f32 = 168.0;
/// Buttons size to their labels — "Close and stop it" does not fit the
/// approval modal's fixed 96px — with a floor so "Cancel" is not a sliver.
const CONFIRM_BTN_MIN: f32 = 84.0;
const CONFIRM_BTN_PAD: f32 = 14.0;

/// The close-a-busy-tab confirm: ⌘W landed on something that is still
/// running, and the three outcomes differ enough to be worth asking.
///
/// Scrim swallows rather than dismisses, on the approval modal's rule — the
/// question exists *because* one of the answers is destructive, and "clicked
/// it away" is not one of the three. Esc is Cancel.
///
/// Drawn under the approval modal and over everything else: this one opened
/// because the user pressed a key, so it may own the window; that one opens on
/// the network's schedule and outranks it.
fn confirm_close_overlay(
    confirm: &super::model::ConfirmCloseModel,
    hover: Option<HitRegion>,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::ConfirmPanel);

    let w = (CONFIRM_W * s).min((m.width - 2.0 * EDGE_PAD * s).max(0.0));
    let h = CONFIRM_H * s;
    let x = (m.width - w) / 2.0;
    let y = ((m.height - h) * 0.32).max(0.0);
    let panel = [x, y, w, h];
    out.rects.push(RectInstance {
        radii: [APPROVAL_RADIUS * s; 4],
        border: colors.line,
        border_width: HAIRLINE * s,
        shadow_blur: 20.0 * s,
        shadow_alpha: colors.shadow_alpha,
        ..RectInstance::filled(panel, colors.panel_bg, no_clip)
    });
    out.hit.push(panel, HitRegion::ConfirmPanel);

    // Three lines, drawn verbatim: the app composed them, because only the app
    // knows what is running and whether there is a daemon to leave it with.
    let pad = APPROVAL_PAD * s;
    let title_px = UI_BODY * s;
    out.texts.push(TextRun {
        text: confirm.title.clone(),
        pos: [x + pad, y + pad + title_px * 0.8],
        max_width: w - 2.0 * pad,
        color: colors.text_active,
        clip: panel,
        px: title_px,
        bold: true,
        tracking: 0.0,
    });

    let body_px = UI_BODY * s;
    if !confirm.body.is_empty() {
        out.texts.push(TextRun {
            text: confirm.body.clone(),
            pos: [x + pad, y + 54.0 * s + body_px * 0.72],
            max_width: w - 2.0 * pad,
            color: colors.text_inactive,
            clip: panel,
            px: body_px,
            bold: false,
            tracking: 0.0,
        });
    }
    let sub_px = UI_STATUS * s;
    out.texts.push(TextRun {
        text: confirm.hint.clone(),
        pos: [x + pad, y + 80.0 * s + sub_px * 0.72],
        max_width: w - 2.0 * pad,
        color: colors.text_faint,
        clip: panel,
        px: sub_px,
        bold: false,
        tracking: 0.0,
    });

    // Right to left, so the corner position — the one every dialog has taught
    // the hand to reach for — holds the answer that destroys nothing. Cancel
    // ends up leftmost, where a misfire is cheapest.
    let btn_h = APPROVAL_BTN_H * s;
    let by = y + h - pad - btn_h;
    let mut right = x + w - pad;
    let mut buttons: Vec<(&str, HitRegion, LinearRgba)> = Vec::with_capacity(3);
    match confirm.choices {
        ConfirmChoices::DetachOrClose => {
            buttons.push(("Detach", HitRegion::ConfirmDetach, colors.accent));
            buttons.push(("Close and stop it", HitRegion::ConfirmClose, colors.danger));
            buttons.push(("Cancel", HitRegion::ConfirmCancel, colors.text_inactive));
        }
        ConfirmChoices::CloseOnly => {
            buttons.push(("Close and stop it", HitRegion::ConfirmClose, colors.danger));
            buttons.push(("Cancel", HitRegion::ConfirmCancel, colors.text_inactive));
        }
        // One button, and it is not destructive. This panel is a *refusal*,
        // not a question: ⌘B asked for something that cannot happen here, and
        // putting "Close and stop it" in the corner would leave the gesture
        // that promised not to end the shell one click from ending it.
        ConfirmChoices::Acknowledge => {
            buttons.push(("OK", HitRegion::ConfirmCancel, colors.accent));
        }
    }
    for (label, region, ink) in buttons {
        let label_px = UI_BODY * s;
        let bw = (measure(label, label_px, false, 0.0) + 2.0 * CONFIRM_BTN_PAD * s)
            .max(CONFIRM_BTN_MIN * s);
        let rect = [right - bw, by, bw, btn_h];
        let hovered = hover == Some(region);
        out.rects.push(RectInstance {
            radii: [7.0 * s; 4],
            border: ink,
            border_width: HAIRLINE * s,
            ..RectInstance::filled(
                rect,
                if hovered { colors.tab_hover_bg } else { colors.panel_bg },
                panel,
            )
        });
        let lw = measure(label, label_px, false, 0.0);
        out.texts.push(TextRun {
            text: label.into(),
            pos: [rect[0] + (bw - lw) / 2.0, baseline_in(by, btn_h, label_px)],
            max_width: bw,
            color: ink,
            clip: panel,
            px: label_px,
            bold: false,
            tracking: 0.0,
        });
        out.hit.push(rect, region);
        right = rect[0] - APPROVAL_BTN_GAP * s;
    }
}

fn notice_bar(
    text: &str,
    area: [f32; 4],
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let px = UI_BODY * s;
    let h = NOTICE_H * s;
    let pad = NOTICE_PAD * s;
    let w = (measure(text, px, false, 0.0) + 2.0 * pad).min((area[2] - 2.0 * EDGE_PAD * s).max(0.0));
    let x = area[0] + (area[2] - w) / 2.0;
    let y = area[1] + NOTICE_TOP * s;
    // The panel fill, like the picker: a see-through bar over a busy grid is
    // unreadable at exactly the moment someone is trying to compare digits.
    // The warn border is the "a person is needed" ink the chips already use.
    out.rects.push(RectInstance {
        radii: [h / 2.0; 4],
        border: colors.warn,
        border_width: HAIRLINE * s,
        ..RectInstance::filled([x, y, w, h], colors.panel_bg, area)
    });
    out.texts.push(TextRun {
        text: text.to_string(),
        pos: [x + pad, baseline_in(y, h, px)],
        max_width: (w - 2.0 * pad).max(0.0),
        color: colors.text_active,
        clip: area,
        px,
        bold: false,
        tracking: 0.0,
    });
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
        // Caret first when the query is empty — over the first letter of the
        // placeholder is where it looked broken.
        let (qtext, qcolor, caret_x, text_x) = if picker.filter.is_empty() {
            ("Search sessions, blocks, hosts".to_string(), colors.text_faint, qx, qx + 14.0 * s)
        } else {
            // Where the caret actually is, not where the text ends: arrowing
            // back into a query has to show you where you are (#251).
            let at = measure(&picker.filter[..picker.filter_caret.at], prompt_px, false, 0.0);
            (picker.filter.clone(), colors.text_active, qx + at + 2.0 * s, qx)
        };
        if let Some((lo, hi)) = picker.filter_caret.selection {
            let (a, b) = (
                measure(&picker.filter[..lo], prompt_px, false, 0.0),
                measure(&picker.filter[..hi], prompt_px, false, 0.0),
            );
            out.rects.push(RectInstance::rounded(
                [qx + a, qy + (qh - 18.0 * s) / 2.0, b - a, 18.0 * s],
                2.0 * s,
                colors.accent_soft,
                no_clip,
            ));
        }
        let qw = measure(&qtext, prompt_px, false, 0.0).min(w * 0.6);
        out.texts.push(TextRun {
            text: qtext,
            pos: [text_x, baseline_in(qy, qh, prompt_px)],
            max_width: qw,
            color: qcolor,
            clip: no_clip,
            px: prompt_px,
            bold: false,
            tracking: 0.0,
        });
        let _ = qx;
        // The caret: an 8×16 accent block on the design's step-end blink.
        if picker.caret_on {
            out.rects.push(RectInstance::filled(
                [caret_x, qy + (qh - 16.0 * s) / 2.0, 8.0 * s, 16.0 * s],
                colors.accent,
                no_clip,
            ));
        }
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
            // §6's own wording, and now its own behaviour: ⇧⏎ re-opens the
            // picker to choose a machine. It read "run in its session" while
            // that was the honest stand-in (#324).
            ("\u{21e7}\u{23ce}", " run on host\u{2026}"),
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

/// The cwd chip's directory browser (#439) — the palette's modality recipe
/// with a listing where the command table was. Rows are uniform, every one
/// selectable; the `..` row (row 0 when the path has a parent) draws faint
/// because it navigates where the rest switch.
/// The "Open file…" prompt (#464), in the palette's shape: a scrim, a rounded
/// panel, one path entry, and a line naming where a relative path will land.
///
/// The `where` line is not decoration. A path prompt in a fleet terminal is
/// ambiguous in two directions at once — which directory, and which *machine*
/// — and neither is recoverable from what the person typed.
fn open_file_overlay(
    model: &super::model::OpenFileModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::OpenFileScrim);

    let w = (PALETTE_W * s).min(m.width - PICKER_MARGIN * s);
    let field_h = m.line_height + 2.0 * PICKER_PAD * s;
    let where_h = m.line_height + PICKER_PAD * s;
    let h = field_h + HAIRLINE * s + where_h;
    let panel = [(m.width - w) / 2.0, (m.height - h) / 2.5, w, h];
    let mut panel_rect = RectInstance::rounded(panel, PICKER_RADIUS * s, colors.panel_bg, no_clip);
    panel_rect.shadow_blur = 24.0 * s;
    panel_rect.shadow_alpha = colors.shadow_alpha;
    out.rects.push(panel_rect);
    // Swallows a near-miss, so a click just outside the entry does not dismiss
    // a path someone has half-typed.
    out.hit.push(panel, HitRegion::OpenFilePanel);

    let (text, color) = if model.path.is_empty() {
        ("path to a file".to_string(), colors.text_faint)
    } else {
        (model.path.clone(), colors.text_active)
    };
    let x = panel[0] + PICKER_PAD * s;
    if let Some((lo, hi)) = model.caret.selection {
        let (a, b) = (
            measure(&model.path[..lo], m.font_px, false, 0.0),
            measure(&model.path[..hi], m.font_px, false, 0.0),
        );
        out.rects.push(RectInstance::rounded(
            [x + a, panel[1] + PICKER_PAD * s, b - a, m.line_height],
            2.0 * s,
            colors.accent_soft,
            panel,
        ));
    }
    out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
        tracking: 0.0,
        text,
        pos: [x, text_baseline(m, panel[1], field_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color,
        clip: panel,
    });
    if !model.path.is_empty() {
        let at = measure(&model.path[..model.caret.at], m.font_px, false, 0.0);
        out.rects.push(RectInstance::filled(
            [x + at, panel[1] + PICKER_PAD * s, (1.5 * s).max(1.0), m.line_height],
            colors.text_active,
            panel,
        ));
    }
    out.rects.push(RectInstance::filled(
        [panel[0], panel[1] + field_h, w, HAIRLINE * s],
        colors.line,
        no_clip,
    ));

    let hint = if model.cwd.is_empty() {
        format!("on {}", model.host)
    } else {
        format!("in {} on {}", model.cwd, model.host)
    };
    out.texts.push(TextRun {
        px: UI_STATUS * s,
        bold: false,
        tracking: 0.0,
        text: hint,
        pos: [x, text_baseline(m, panel[1] + field_h, where_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color: colors.text_faint,
        clip: panel,
    });
}

fn dir_picker_overlay(
    picker: &super::model::DirPickerModel,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    out.rects.push(RectInstance::filled(no_clip, colors.scrim, no_clip));
    out.hit.push(no_clip, HitRegion::DirPickerScrim);

    let w = (PALETTE_W * s).min(m.width - PICKER_MARGIN * s);
    let filter_h = m.line_height + 2.0 * PICKER_PAD * s;
    // Sized to what it holds, capped at the palette's height: a listing of
    // eight directories in a panel built for forty reads as half-loaded,
    // and the empty states below need one row, not a screenful of nothing.
    let listed = picker.rows.len().max(1) as f32;
    // The truncation note gets a row of its own; drawn into the listing's
    // space it would sit on top of the last directory, since the padding is
    // a third of a row tall.
    let footer_h = if picker.truncated { PALETTE_ROW_H * s } else { 0.0 };
    let wanted =
        filter_h + HAIRLINE * s + listed * PALETTE_ROW_H * s + footer_h + PICKER_PAD * s;
    let h = wanted.min((PALETTE_H * s).min(m.height - PICKER_MARGIN * s));
    let panel = [(m.width - w) / 2.0, (m.height - h) / 2.5, w, h];
    let mut panel_rect = RectInstance::rounded(panel, PICKER_RADIUS * s, colors.panel_bg, no_clip);
    panel_rect.shadow_blur = 24.0 * s;
    panel_rect.shadow_alpha = colors.shadow_alpha;
    out.rects.push(panel_rect);
    out.hit.push(panel, HitRegion::DirPickerPanel);

    let (filter_text, filter_color) = if picker.filter.is_empty() {
        ("search directories".to_string(), colors.text_faint)
    } else {
        (picker.filter.clone(), colors.text_active)
    };
    let filter_x = panel[0] + PICKER_PAD * s;
    if let Some((lo, hi)) = picker.filter_caret.selection {
        let (a, b) = (
            measure(&picker.filter[..lo], m.font_px, false, 0.0),
            measure(&picker.filter[..hi], m.font_px, false, 0.0),
        );
        out.rects.push(RectInstance::rounded(
            [filter_x + a, panel[1] + PICKER_PAD * s, b - a, m.line_height],
            2.0 * s,
            colors.accent_soft,
            panel,
        ));
    }
    out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
        tracking: 0.0,
        text: filter_text,
        pos: [filter_x, text_baseline(m, panel[1], filter_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color: filter_color,
        clip: panel,
    });
    if !picker.filter.is_empty() {
        let at = measure(&picker.filter[..picker.filter_caret.at], m.font_px, false, 0.0);
        out.rects.push(RectInstance::filled(
            [filter_x + at, panel[1] + PICKER_PAD * s, (1.5 * s).max(1.0), m.line_height],
            colors.text_active,
            panel,
        ));
    }
    out.rects.push(RectInstance::filled(
        [panel[0], panel[1] + filter_h, w, HAIRLINE * s],
        colors.line,
        no_clip,
    ));

    let rows_clip = [
        panel[0],
        panel[1] + filter_h + HAIRLINE * s,
        w,
        (h - filter_h - HAIRLINE * s - footer_h).max(0.0),
    ];

    // The three empty states, each saying which it is: an answer in flight,
    // a listing that was refused, and a filter that matches nothing.
    let notice = if picker.loading {
        Some(("listing\u{2026}".to_string(), colors.text_faint))
    } else if !picker.error.is_empty() {
        Some((picker.error.clone(), colors.danger))
    } else if picker.rows.is_empty() && !picker.filter.is_empty() {
        Some((format!("nothing matches \u{201c}{}\u{201d}", picker.filter), colors.text_faint))
    } else if picker.rows.is_empty() {
        Some(("no subdirectories".to_string(), colors.text_faint))
    } else {
        None
    };
    if let Some((text, color)) = notice {
        out.texts.push(TextRun {
            px: m.font_px,
            bold: false,
            tracking: 0.0,
            text,
            pos: [panel[0] + PICKER_PAD * s, text_baseline(m, rows_clip[1], PALETTE_ROW_H * s)],
            max_width: w - 2.0 * PICKER_PAD * s,
            color,
            clip: rows_clip,
        });
    }

    let row_h = PALETTE_ROW_H * s;
    let content_h = picker.rows.len() as f32 * row_h;
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let mut scroll = picker.scroll.clamp(0.0, max_scroll);
    if picker.ensure_visible && picker.selected < picker.rows.len() {
        let top = picker.selected as f32 * row_h;
        let bottom = top + row_h;
        if top < scroll {
            scroll = top;
        } else if bottom > scroll + rows_clip[3] {
            scroll = bottom - rows_clip[3];
        }
        scroll = scroll.clamp(0.0, max_scroll);
    }
    out.dir_picker_scroll = scroll;

    let left = panel[0] + PICKER_PAD * s;
    for (i, label) in picker.rows.iter().enumerate() {
        let y = rows_clip[1] + i as f32 * row_h - scroll;
        let band = [panel[0], y, w, row_h];
        let Some(visible) = intersect(band, rows_clip) else { continue };

        if i == picker.selected {
            let chip = [panel[0] + 4.0 * s, y + 1.0 * s, w - 8.0 * s, row_h - 2.0 * s];
            out.rects.push(RectInstance::rounded(chip, RADIUS * s, colors.accent_soft, rows_clip));
        }
        out.hit.push(visible, HitRegion::DirPickerRow(i));
        let parent_row = picker.has_parent && i == 0;
        out.texts.push(TextRun {
            px: m.font_px,
            bold: false,
            tracking: 0.0,
            text: label.clone(),
            pos: [left, text_baseline(m, y, row_h)],
            max_width: w - 2.0 * PICKER_PAD * s,
            color: if parent_row { colors.text_faint } else { colors.text_inactive },
            clip: rows_clip,
        });
    }

    if picker.truncated {
        // Said, not silently cut: a listing that looks complete reads as
        // "covered everything" when it didn't.
        out.texts.push(TextRun {
            px: m.font_px,
            bold: false,
            tracking: 0.0,
            text: "more not shown \u{2014} narrow the search".to_string(),
            pos: [left, text_baseline(m, panel[1] + h - row_h, row_h)],
            max_width: w - 2.0 * PICKER_PAD * s,
            color: colors.text_faint,
            clip: panel,
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
    let filter_x = panel[0] + PICKER_PAD * s;
    if let Some((lo, hi)) = palette.filter_caret.selection {
        let (a, b) = (
            measure(&palette.filter[..lo], m.font_px, false, 0.0),
            measure(&palette.filter[..hi], m.font_px, false, 0.0),
        );
        out.rects.push(RectInstance::rounded(
            [filter_x + a, panel[1] + PICKER_PAD * s, b - a, m.line_height],
            2.0 * s,
            colors.accent_soft,
            panel,
        ));
    }
    out.texts.push(TextRun {
        px: m.font_px,
        bold: false,
            tracking: 0.0,
        text: filter_text,
        pos: [filter_x, text_baseline(m, panel[1], filter_h)],
        max_width: w - 2.0 * PICKER_PAD * s,
        color: filter_color,
        clip: panel,
    });
    // The caret, once there is something to put it in. Over an empty
    // placeholder it reads as a stray line rather than as a cursor.
    if !palette.filter.is_empty() {
        let at = measure(&palette.filter[..palette.filter_caret.at], m.font_px, false, 0.0);
        out.rects.push(RectInstance::filled(
            [filter_x + at, panel[1] + PICKER_PAD * s, (1.5 * s).max(1.0), m.line_height],
            colors.text_active,
            panel,
        ));
    }
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

// The + launcher menu (design §1), logical px.
const LAUNCHER_W: f32 = 318.0;
const LAUNCHER_RADIUS: f32 = 12.0;
const LAUNCHER_PAD: f32 = 6.0;
/// The `⏎ runs the default` header line.
const LAUNCHER_HEAD_H: f32 = 24.0;
/// A profile row: name over its command line.
const LAUNCHER_ROW_H: f32 = 44.0;
const LAUNCHER_DIVIDER_H: f32 = 9.0;
/// The two single-line action rows.
const LAUNCHER_ACTION_H: f32 = 32.0;
/// Vertical gap between the `+` and the panel's top edge.
const LAUNCHER_DROP: f32 = 6.0;
/// The panel never touches the window edge — the clamp the layout test pins.
const LAUNCHER_MARGIN: f32 = 8.0;

fn launcher_overlay(
    launcher: &super::model::LauncherModel,
    hover: Option<HitRegion>,
    colors: &ChromeColors,
    m: &ChromeMetrics,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    use super::model::{LauncherAnchor, LauncherRow};
    let s = m.scale;
    let no_clip = [0.0, 0.0, m.width, m.height];

    // The scrim: full-window and *transparent* — a menu dims nothing, but a
    // click-away must dismiss without falling through to a tab, the grid,
    // or a block header. A hit region with no rect is exactly that.
    out.hit.push(no_clip, HitRegion::LauncherScrim);

    let w = (LAUNCHER_W * s).min((m.width - 2.0 * LAUNCHER_MARGIN * s).max(0.0));
    let nt = out.new_tab_rect;
    let (x, y) = match launcher.anchor {
        // Right-anchored under the button (§1). The + sits at the strip's
        // right end, so this fits by construction — the clamp below is what
        // makes "by construction" survive a narrow window anyway.
        LauncherAnchor::Strip => (nt[0] + nt[2] - w, nt[1] + nt[3] + LAUNCHER_DROP * s),
        // top:40, left:0 from the button, opening rightwards over the pane
        // (§2): right-anchored it runs off the window's left edge.
        LauncherAnchor::Sidebar => (nt[0], nt[1] + 40.0 * s),
    };
    let x = x.clamp(LAUNCHER_MARGIN * s, (m.width - w - LAUNCHER_MARGIN * s).max(LAUNCHER_MARGIN * s));

    let row_h = |row: &LauncherRow| match row {
        LauncherRow::Profile { .. } => LAUNCHER_ROW_H * s,
        // The sidebar's host-header height, because it is the same header
        // saying the same thing in the same treatment.
        LauncherRow::Group { .. } => GROUP_HEADER_H * s,
        LauncherRow::Divider => LAUNCHER_DIVIDER_H * s,
        LauncherRow::RunOnHost | LauncherRow::ManageProfiles { .. } => LAUNCHER_ACTION_H * s,
    };
    let content_h: f32 = LAUNCHER_HEAD_H * s
        + launcher.rows.iter().map(&row_h).sum::<f32>()
        + 2.0 * LAUNCHER_PAD * s;
    // Clamped to the window; a menu long enough to clip earns scrolling with
    // the profiles editor's work item, not here — rows past the edge are
    // clipped from the hit map too, so nothing invisible answers clicks.
    let h = content_h.min((m.height - y - LAUNCHER_MARGIN * s).max(0.0));
    let panel = [x, y, w, h];
    out.rects.push(RectInstance {
        radii: [LAUNCHER_RADIUS * s; 4],
        border: colors.line,
        border_width: HAIRLINE * s,
        // The design's `0 20px 50px rgba(0,0,0,.62)`, through the theme's
        // shadow discipline like the picker.
        shadow_blur: 20.0 * s,
        shadow_alpha: colors.shadow_alpha,
        ..RectInstance::filled(panel, colors.panel_bg, no_clip)
    });
    // The panel between rows swallows clicks; rows out-rank it where they
    // overlap (push order is draw order).
    out.hit.push(panel, HitRegion::LauncherPanel);

    // Header: `⏎ runs the default`, small and faint.
    {
        let head = [x, y + LAUNCHER_PAD * s, w, LAUNCHER_HEAD_H * s];
        out.texts.push(TextRun {
            text: "\u{23ce} runs the default".into(),
            pos: [x + 12.0 * s, baseline_in(head[1], head[3], UI_CHORD * s)],
            max_width: w - 24.0 * s,
            color: colors.text_faint,
            clip: panel,
            px: UI_CHORD * s,
            bold: false,
            tracking: 0.0,
        });
    }

    let mut ry = y + LAUNCHER_PAD * s + LAUNCHER_HEAD_H * s;
    for (i, row) in launcher.rows.iter().enumerate() {
        let rh = row_h(row);
        let rect = [x + LAUNCHER_PAD * s, ry, w - 2.0 * LAUNCHER_PAD * s, rh];
        ry += rh;

        if matches!(row, LauncherRow::Divider) {
            out.rects.push(RectInstance::filled(
                [rect[0] + 4.0 * s, rect[1] + rh / 2.0, rect[2] - 8.0 * s, HAIRLINE * s],
                colors.hairline_soft,
                panel,
            ));
            continue;
        }

        // A group header names the machine the rows under it will run on
        // (#268). Drawn exactly as the vertical sidebar's host headers are —
        // dot, uppercase tracked label, mono sub — because it is the same
        // header saying the same thing, and two treatments for one idea is
        // how a window stops looking like one program.
        //
        // No hit region and no selection: `continue` before either, so the
        // keyboard skips it (its action is `None`) and a click falls through
        // to the panel, which swallows it.
        if let LauncherRow::Group { label, sub, online } = row {
            let cy = rect[1] + rh / 2.0;
            let dot_x = rect[0] + 8.0 * s + DOT * s / 2.0;
            let ink = if *online { colors.text_inactive } else { colors.text_faint };
            dot(&mut out.rects, dot_x, cy, DOT * s, ink, panel);

            let text = label.to_uppercase();
            let label_px = UI_STATUS * s;
            let tracking = 0.09 * label_px;
            let label_w = measure(&text, label_px, true, tracking);
            let label_x = dot_x + DOT * s / 2.0 + 7.0 * s;
            out.texts.push(TextRun {
                text,
                pos: [label_x, baseline_in(rect[1], rh, label_px)],
                max_width: label_w + 2.0,
                color: ink,
                clip: panel,
                px: label_px,
                bold: true,
                tracking,
            });
            if !sub.is_empty() {
                let sub_px = UI_CHORD * s;
                out.texts.push(TextRun {
                    text: sub.clone(),
                    pos: [label_x + label_w + 7.0 * s, baseline_in(rect[1], rh, sub_px)],
                    max_width: (rect[0] + rect[2] - label_x - label_w - 14.0 * s).max(0.0),
                    color: colors.text_faint,
                    clip: panel,
                    px: sub_px,
                    bold: false,
                    tracking: 0.0,
                });
            }
            continue;
        }

        let selected = i == launcher.selected;
        let hovered = hover == Some(HitRegion::LauncherRow(i));
        // Selected rows read accentSoft, hovered ones selSoft (§1) — the
        // same pair every row list in the design uses.
        if selected {
            out.rects.push(RectInstance::rounded(rect, 8.0 * s, colors.accent_soft, panel));
        } else if hovered {
            out.rects.push(RectInstance::rounded(rect, 8.0 * s, colors.tab_hover_bg, panel));
        }

        match row {
            // Both handled above, before the selection wash and the hit
            // region — neither is a row you can land on.
            LauncherRow::Divider | LauncherRow::Group { .. } => {}
            LauncherRow::Profile { name, command, host_label, default, digit, active, accent } => {
                // Glyph tile: the row's accent on a 12%-alpha wash of it —
                // the same recipe as the tab chips, so a profile looks like
                // the tab it is about to become.
                let ink = accent_color(colors, *accent);
                let tile = [
                    rect[0] + 6.0 * s,
                    rect[1] + (rh - TILE * s) / 2.0,
                    TILE * s,
                    TILE * s,
                ];
                out.rects.push(RectInstance::rounded(tile, TILE_RADIUS * s, washed(ink, 0.12), panel));
                dot(&mut out.rects, tile[0] + tile[2] / 2.0, tile[1] + tile[3] / 2.0, DOT * s, ink, panel);

                let tx = tile[0] + TILE * s + TAB_INNER_GAP * s;
                let mut right = rect[0] + rect[2] - 12.0 * s;
                if let Some(d) = digit {
                    let hint = d.to_string();
                    let hw = measure(&hint, UI_CHORD * s, false, 0.0);
                    out.texts.push(TextRun {
                        text: hint,
                        pos: [right - hw, baseline_in(rect[1], rh, UI_CHORD * s)],
                        max_width: hw + 2.0,
                        color: colors.text_faint,
                        clip: panel,
                        px: UI_CHORD * s,
                        bold: false,
                        tracking: 0.0,
                    });
                    right -= hw + 8.0 * s;
                }

                // The host chip, left of the digit: drawn only when the
                // profile pins a machine, because the launch now honours the
                // pin (issue #175) — a bordered pill, text not colour alone,
                // per the design's origin rule.
                if let Some(host) = host_label {
                    let chip_px = 10.0 * s;
                    let tw = measure(host, chip_px, false, 0.0);
                    let chip_h = 16.0 * s;
                    let chip = [
                        right - tw - 12.0 * s,
                        rect[1] + (rh - chip_h) / 2.0,
                        tw + 12.0 * s,
                        chip_h,
                    ];
                    out.rects.push(RectInstance {
                        radii: [4.0 * s; 4],
                        border: colors.line,
                        border_width: HAIRLINE * s,
                        ..RectInstance::filled(chip, LinearRgba::TRANSPARENT, panel)
                    });
                    out.texts.push(TextRun {
                        text: host.clone(),
                        pos: [chip[0] + 6.0 * s, baseline_in(chip[1], chip[3], chip_px)],
                        max_width: tw + 2.0,
                        color: colors.text_inactive,
                        clip: panel,
                        px: chip_px,
                        bold: false,
                        tracking: 0.0,
                    });
                    right -= chip[2] + 8.0 * s;
                }

                let name_px = UI_BODY * s;
                let name_w = measure(name, name_px, false, 0.0).min((right - tx).max(0.0));
                out.texts.push(TextRun {
                    text: name.clone(),
                    pos: [tx, baseline_in(rect[1] + 2.0 * s, 20.0 * s, name_px)],
                    max_width: name_w,
                    // An active profile reads accent, like the app-tab rows
                    // in the mock — the menu says "this is where you are".
                    color: if *active { colors.accent } else { colors.text_active },
                    clip: panel,
                    px: name_px,
                    bold: false,
                    tracking: 0.0,
                });
                if *default {
                    // The `default` tag on accentSoft (§1) — what ⏎ runs.
                    let tag_px = 9.0 * s;
                    let tw = measure("default", tag_px, false, 0.0);
                    let chip = [tx + name_w + 6.0 * s, rect[1] + 6.0 * s, tw + 10.0 * s, 14.0 * s];
                    if chip[0] + chip[2] < right {
                        out.rects.push(RectInstance::rounded(chip, 4.0 * s, colors.accent_soft, panel));
                        out.texts.push(TextRun {
                            text: "default".into(),
                            pos: [chip[0] + 5.0 * s, baseline_in(chip[1], chip[3], tag_px)],
                            max_width: tw + 2.0,
                            color: colors.accent,
                            clip: panel,
                            px: tag_px,
                            bold: false,
                            tracking: 0.0,
                        });
                    }
                }
                out.texts.push(TextRun {
                    text: command.clone(),
                    pos: [tx, baseline_in(rect[1] + 22.0 * s, 18.0 * s, UI_CHORD * s)],
                    max_width: (right - tx).max(0.0),
                    color: colors.text_faint,
                    clip: panel,
                    px: UI_CHORD * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            LauncherRow::RunOnHost | LauncherRow::ManageProfiles { .. } => {
                let (label, hint) = match row {
                    LauncherRow::RunOnHost => ("Run on another host\u{2026}", "\u{21e7}\u{23ce}".to_string()),
                    LauncherRow::ManageProfiles { chord } => ("Manage profiles", chord.clone()),
                    _ => unreachable!("matched above"),
                };
                let mut right = rect[0] + rect[2] - 12.0 * s;
                if !hint.is_empty() {
                    let hw = measure(&hint, UI_CHORD * s, false, 0.0);
                    out.texts.push(TextRun {
                        text: hint,
                        pos: [right - hw, baseline_in(rect[1], rh, UI_CHORD * s)],
                        max_width: hw + 2.0,
                        color: colors.text_faint,
                        clip: panel,
                        px: UI_CHORD * s,
                        bold: false,
                        tracking: 0.0,
                    });
                    right -= hw + 8.0 * s;
                }
                out.texts.push(TextRun {
                    text: label.into(),
                    pos: [rect[0] + 12.0 * s, baseline_in(rect[1], rh, UI_BODY * s)],
                    max_width: (right - rect[0] - 12.0 * s).max(0.0),
                    color: if selected { colors.text_active } else { colors.text_inactive },
                    clip: panel,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
        }

        // Only rows that reach here are clickable — the divider and the group
        // headers `continue` above, so neither takes a hit region and neither
        // can be landed on.
        if let Some(hit) = intersect(rect, panel) {
            out.hit.push(hit, HitRegion::LauncherRow(i));
        }
    }
}

pub(super) fn intersect(r: [f32; 4], c: [f32; 4]) -> Option<[f32; 4]> {
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
    let reserve = model.controls.native_leading.map_or(BAR_PAD * s, |t| t[0] + TRAFFIC_PAD * s);
    out.hit.push([0.0, 0.0, reserve, sh], HitRegion::Drag);

    // Right side first: the pill has an intrinsic width, the tabs take what
    // remains. One pill — the palette's "(chord)" — 26px tall, hairline
    // border that turns accent under the pointer. The layout toggle is gone
    // from the chrome (design §1): `tabs.position` is a setting, and a button
    // duplicating a setting is a second source of truth.
    //
    // The caption cluster, when we draw one, is further right still and the
    // pill starts from where it ends. Both are right-aligned, so laying them
    // out independently is how they would come to overlap.
    let pill_y = (sh - PILL_H * s) / 2.0;
    let caption_left = caption_cluster(&mut out, colors, model, [0.0, 0.0, m.width, sh], s, measure);
    let mut right = caption_left - BAR_PAD * s;
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
        right = rect[0] - BAR_PAD * s;
    }

    let avail = (right - reserve).max(0.0);
    let chip_y = sh - TAB_H * s;

    let gap = TAB_GAP * s;
    let new_tab_w = NEW_TAB_W * s;
    // The `+` sits *outside* the scrolled row (design §1: "the launcher must
    // sit outside the scrolling row or its menu is clipped"), so the row's
    // viewport ends where the button's slot begins.
    // No extra gap subtracted: content_w already carries a trailing gap per
    // chip, so that is what separates the last chip from the `+` — taking a
    // second one here left the button a gap short of the strip's right edge
    // whenever the row overflowed.
    let viewport = (avail - new_tab_w).max(0.0);

    // Widths per chip. App tabs (Settings, Profiles) size to their content —
    // glyph, label and the same × every other chip carries (#494); session
    // chips share what is left at the 168px basis, shrink to the 104px
    // floor, and past that the row scrolls rather than crushing — which is
    // what makes the #51 class of overlap impossible.
    let widths: Vec<f32> = {
        let mut app_total = 0.0;
        let mut sessions = 0usize;
        let natural: Vec<f32> = model
            .tabs
            .iter()
            .map(|tab| match tab.kind {
                TabKind::Session => {
                    sessions += 1;
                    0.0
                }
                TabKind::Settings | TabKind::Profiles => {
                    let label_w = measure(&tab.title, UI_BODY * s, false, 0.0);
                    let w = (2.0 * TAB_PAD * s
                        + TILE * s
                        + TAB_INNER_GAP * s
                        + label_w
                        + TAB_INNER_GAP * s
                        + CLOSE * s)
                        .min(TAB_MAX * s);
                    app_total += w;
                    w
                }
            })
            .collect();
        let n = model.tabs.len();
        let share = if sessions == 0 {
            0.0
        } else {
            (viewport - app_total - n as f32 * gap) / sessions as f32
        };
        let session_w = share.clamp(TAB_MIN * s, TAB_BASIS * s);
        natural.into_iter().map(|w| if w == 0.0 { session_w } else { w }).collect()
    };
    let content_w: f32 = widths.iter().map(|w| w + gap).sum();
    let max_scroll = (content_w - viewport).max(0.0);
    let mut scroll = model.strip_scroll.clamp(0.0, max_scroll);

    // Activation must never land on a chip the user cannot see — the
    // picker's ensure-visible discipline, applied to the strip.
    if model.ensure_active_visible {
        if let Some(active_w) = widths.get(model.active) {
            let x0: f32 = widths[..model.active].iter().map(|w| w + gap).sum();
            let x1 = x0 + active_w;
            if x0 < scroll {
                scroll = x0;
            } else if x1 > scroll + viewport {
                scroll = x1 - viewport;
            }
            scroll = scroll.clamp(0.0, max_scroll);
        }
    }
    out.strip_scroll = scroll;

    // Chips reach the strip's bottom edge and cover the hairline there, so
    // the active tab's fill meets the pane with nothing drawn between them.
    // They clip at the row's viewport, not the whole strip: a scrolled chip
    // must not slide under the `+`.
    let clip = [reserve, 0.0, viewport, sh];

    let mut chip_x = reserve - out.strip_scroll;
    for (i, tab) in model.tabs.iter().enumerate() {
        let tab_w = widths[i];
        let x = chip_x;
        chip_x += tab_w + gap;
        let chip = [x, chip_y, tab_w, TAB_H * s];

        let active = i == model.active;
        let hovered = model.hover == Some(HitRegion::Tab(tab.addr));
        // A session chip's rule and glyph take the tab's own accent (§12);
        // an app tab is a place, not a shell — it takes `ui.accent`, per
        // §11's "2px ui.accent inset top rule".
        let chip_accent = if tab.kind == TabKind::Session {
            accent_color(colors, tab.tab_accent)
        } else {
            colors.accent
        };
        // What is actually behind the chip's glyph tile, which the progress
        // ring's bite has to match to read as a gap rather than a notch.
        // Three answers, and the same three the fills below produce.
        let chip_bg = if active {
            colors.tab_active_bg
        } else if hovered {
            colors.tab_hover_bg
        } else {
            colors.strip_bg
        };
        if active {
            // The mock's recipe, translated from CSS: `border: 1px line` with
            // `border-bottom: none`, and `box-shadow: inset 0 2px 0 accent` —
            // the accent hugs the rounded top edge and *thins to nothing* as
            // each corner turns vertical. Three SDF rects:
            //
            // 1. The border ring, with its bottom side omitted. It used to be
            //    drawn one hairline taller so the bottom edge fell outside the
            //    strip clip; the pipeline can say "no bottom border" now, so
            //    the rect is the chip.
            out.rects.push(RectInstance {
                radii: [TAB_RADIUS * s, TAB_RADIUS * s, 0.0, 0.0],
                border: colors.line,
                border_width: HAIRLINE * s,
                border_omit: border_sides::BOTTOM,
                ..RectInstance::filled(chip, LinearRgba::TRANSPARENT, clip)
            });
            // 2. The fill, inside the border, running to the strip's bottom
            //    edge so the chip meets the pane with nothing drawn between.
            out.rects.push(RectInstance {
                radii: [(TAB_RADIUS - HAIRLINE) * s, (TAB_RADIUS - HAIRLINE) * s, 0.0, 0.0],
                ..RectInstance::filled(
                    [
                        chip[0] + HAIRLINE * s,
                        chip[1] + HAIRLINE * s,
                        chip[2] - 2.0 * HAIRLINE * s,
                        chip[3] - HAIRLINE * s,
                    ],
                    colors.tab_active_bg,
                    clip,
                )
            });
            // 3. The inset accent: a 2px stroke on the fill's geometry with
            //    every side but the top omitted, so it traces the top edge and
            //    thins away into both curves exactly as the inset shadow does.
            //    This used to be a full ring *clipped* to the top
            //    `TAB_RADIUS + ACCENT_RULE` band, which is not the same picture
            //    at all: a clip cuts a full-weight stroke off square, leaving
            //    two stubs running down the chip's sides. Sitting one hairline
            //    inside the ring keeps the `ui.line` border visible above it,
            //    which is where an inset shadow sits relative to a border.
            //    In the tab's own accent (§12): the rule is the chrome's one
            //    per-tab concession, and it must agree with the glyph tile
            //    below or the chip names two identities at once.
            out.rects.push(RectInstance {
                radii: [(TAB_RADIUS - HAIRLINE) * s, (TAB_RADIUS - HAIRLINE) * s, 0.0, 0.0],
                border: chip_accent,
                border_width: ACCENT_RULE * s,
                border_omit: border_sides::RIGHT | border_sides::BOTTOM | border_sides::LEFT,
                ..RectInstance::filled(
                    [
                        chip[0] + HAIRLINE * s,
                        chip[1] + HAIRLINE * s,
                        chip[2] - 2.0 * HAIRLINE * s,
                        chip[3],
                    ],
                    LinearRgba::TRANSPARENT,
                    clip,
                )
            });
        } else if hovered {
            out.rects.push(RectInstance {
                radii: [TAB_RADIUS * s, TAB_RADIUS * s, 0.0, 0.0],
                ..RectInstance::filled(chip, colors.tab_hover_bg, clip)
            });
        }
        if let Some(hit) = intersect(chip, clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        // The glyph tile: the tab's colour on a 12%-alpha wash of itself,
        // faint with no wash when inactive. Link degradation surfaces here —
        // the one fact the deleted status bar owned alone — as warn (stalled)
        // or danger (reconnecting) ink, active or not: a background tab that
        // is quietly buffering must still say so. App tabs are places, not
        // shells: no wash, no link, faint until active.
        let tile = [
            x + TAB_PAD * s,
            chip_y + (TAB_H - TILE) * s / 2.0,
            TILE * s,
            TILE * s,
        ];
        let ink = match (tab.kind, tab.link) {
            (TabKind::Session, LinkKind::Stalled) => colors.warn,
            (TabKind::Session, LinkKind::Reconnecting) => colors.danger,
            _ if active => chip_accent,
            _ => colors.text_faint,
        };
        if active && tab.kind == TabKind::Session {
            out.rects.push(RectInstance::rounded(tile, TILE_RADIUS * s, washed(ink, 0.12), clip));
        }
        if tab.kind == TabKind::Session {
            // The icon is a placeholder dot until profiles carry real
            // glyphs; the tile's box is what the design fixes (§Assets).
            dot(
                &mut out.rects,
                tile[0] + tile[2] / 2.0,
                tile[1] + tile[3] / 2.0,
                DOT * s,
                ink,
                clip,
            );
            // What the session says about itself, ringed around the tile.
            // Separate from the dot inside it rather than replacing it: the
            // dot is the tab's *identity* (its profile, its host, and the
            // link's health), and a mark that had to choose between "which
            // machine is this" and "is it busy" would be answering the less
            // urgent question half the time.
            //
            // Two sources, and neither implies the other: `running` is the
            // shell's word (OSC 133, so silent under bash and under a TUI),
            // `progress` is the program's own (OSC 9;4). A tab with either is
            // busy; a tab with both draws the more specific one.
            let progress_ink = match tab.progress {
                zest_core::Progress::At { state: zest_core::ProgressState::Error, .. } => {
                    Some(colors.danger)
                }
                zest_core::Progress::At { state: zest_core::ProgressState::Warning, .. } => {
                    Some(colors.warn)
                }
                zest_core::Progress::At { .. } | zest_core::Progress::Indeterminate => {
                    Some(chip_accent)
                }
                zest_core::Progress::None => tab.running.then_some(colors.warn),
            };
            if let Some(ink) = progress_ink {
                let style = match tab.progress {
                    zest_core::Progress::At { percent, .. } => {
                        RingStyle::Arc(f32::from(percent) / 100.0)
                    }
                    // A shell-reported command and an indeterminate bar are
                    // the same fact — busy, no idea how far — and get the
                    // same spinner. A *determinate* one never does: at 100%
                    // it would be a closed ring that still turned, which is
                    // the one state meaning finished drawn as the one meaning
                    // still going.
                    _ => RingStyle::Spin(model.anim.spin),
                };
                ring(
                    &mut out.rects,
                    [tile[0] - 2.0 * s, tile[1] - 2.0 * s, tile[2] + 4.0 * s, tile[3] + 4.0 * s],
                    ink,
                    chip_bg,
                    style,
                    clip,
                );
            }

            // The attention badge (#383), on the tile's top-right corner.
            // A badge rather than recolouring the dot: the dot's ink already
            // carries `LinkKind` degradation, and one mark cannot honestly say
            // two things — a stalled link on a tab that also rang would have
            // to pick which fact to tell you.
            if tab.attention.is_some() {
                dot(
                    &mut out.rects,
                    tile[0] + tile[2],
                    tile[1],
                    BADGE * s,
                    colors.info,
                    clip,
                );
            }
        } else {
            // App tabs carry their own glyph (§11: ⚙ + "Settings"; §12: ▤ +
            // "Profiles") — both BMP, reached by ordinary font fallback,
            // unlike PUA icons. One glyph each rather than a shared ⚙: two
            // chips that look the same are two chips you have to read.
            let gear_px = 12.0 * s;
            let glyph = app_tab_glyph(tab.kind);
            let gw = measure(glyph, gear_px, false, 0.0);
            out.texts.push(TextRun {
                text: glyph.into(),
                pos: [
                    tile[0] + (tile[2] - gw) / 2.0,
                    baseline_in(tile[1], tile[3], gear_px),
                ],
                max_width: tile[2] + 2.0,
                color: ink,
                clip,
                px: gear_px,
                bold: false,
                tracking: 0.0,
            });
        }

        // Every chip carries the close affordance, as the mock draws it —
        // app tabs included since #494. Closing Settings was always "closing
        // a tab", and a tab whose only close is an unadvertised chord or a
        // middle-click is not one you can point at.
        let text_right = {
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
            close[0] - TAB_INNER_GAP * s
        };

        // The title only (design §1): host and cwd were a second line once,
        // and a 34px chip made them unreadable at 9.5px. They live in the
        // vertical sidebar and header now. Unreachability still gets words,
        // not colour alone (#23) — appended to the one line that remains.
        let text_x = tile[0] + TILE * s + TAB_INNER_GAP * s;
        let title = if tab.presence == TabPresence::Unreachable {
            format!("{} · unreachable", tab.title)
        } else {
            tab.title.clone()
        };
        let title_color = match (active, model.focused, tab.connecting) {
            (_, _, true) => colors.text_faint,
            (true, true, _) => colors.text_active,
            _ => colors.text_inactive,
        };
        out.texts.push(TextRun {
            text: title,
            pos: [text_x, baseline_in(chip_y, TAB_H * s, UI_BODY * s)],
            max_width: (text_right - text_x).max(0.0),
            color: title_color,
            clip,
            px: UI_BODY * s,
            bold: false,
            tracking: 0.0,
        });
    }

    // The new-tab button sits after the scrolled row and outside its scroll
    // offset: its future menu must never be clipped by the row, and it must
    // never leave the strip however many tabs are open.
    let nt_x = reserve + content_w.min(viewport);
    let nt = [nt_x, chip_y + (TAB_H * s - NEW_TAB_H * s) / 2.0, NEW_TAB_W * s, NEW_TAB_H * s];
    out.new_tab_rect = nt;
    // While its menu is open the `+` wears selSoft fill and accent ink
    // (design §1) — the open state must read on the button itself, or the
    // menu appears anchored to nothing.
    let open = model.launcher.is_some();
    if open || model.hover == Some(HitRegion::NewTab) {
        out.rects.push(RectInstance::rounded(nt, PILL_RADIUS * s, colors.tab_hover_bg, no_clip));
    }
    if let Some(hit) = intersect(nt, [reserve, 0.0, avail, sh]) {
        out.hit.push(hit, HitRegion::NewTab);
    }
    let plus_w = measure("+", 16.0 * s, false, 0.0);
    out.texts.push(TextRun {
        text: "+".into(),
        pos: [nt[0] + (nt[2] - plus_w) / 2.0, baseline_in(nt[1], nt[3], 16.0 * s)],
        max_width: nt[2],
        color: if open { colors.accent } else { colors.text_inactive },
        clip: [reserve, 0.0, avail, sh],
        px: 16.0 * s,
        bold: false,
        tracking: 0.0,
    });

    // Whatever the content does not cover is a drag handle, like any titlebar.
    let drag_from = (nt[0] + nt[2] + 2.0 * s).min(right);
    if drag_from < right {
        out.hit.push([drag_from, 0.0, right - drag_from, sh], HitRegion::Drag);
    }

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

    // The window is a column (design §2): one full-width header, then the
    // sidebar + pane row. Full width on purpose — on Windows the caption
    // buttons sit top-right, and a header stopping at the sidebar's edge
    // left a dead gap above the sidebar; full width also gives the window
    // controls and ⌘K somewhere to live.
    let header_h =
        model.controls.native_leading.map_or(HEADER_H * s, |t| t[1].max(HEADER_H * s));
    let bar = [0.0, 0.0, m.width, header_h];
    out.rects.push(RectInstance::filled(bar, colors.strip_bg, no_clip));
    out.rects.push(RectInstance::filled(
        [0.0, header_h - HAIRLINE * s, m.width, HAIRLINE * s],
        colors.line,
        no_clip,
    ));
    out.hit.push(bar, HitRegion::Drag);

    // Right end first: the caption cluster when we draw one, then ⌘K — the
    // same helper and order as the horizontal strip, so the two tab
    // positions cannot put Close in two different places.
    let caption_left = caption_cluster(&mut out, colors, model, bar, s, measure);
    let controls_left = {
        let w = measure(&model.palette_chord, UI_CHORD * s, false, 0.0) + 2.0 * PILL_PAD * s;
        let rect =
            [caption_left - BAR_PAD * s - w, (header_h - PILL_H * s) / 2.0, w, PILL_H * s];
        let hovered = model.hover == Some(HitRegion::PalettePill);
        pill_button(&mut out.rects, colors, rect, PILL_RADIUS * s, hovered, no_clip);
        out.hit.push(rect, HitRegion::PalettePill);
        out.texts.push(TextRun {
            text: model.palette_chord.clone(),
            pos: [rect[0] + PILL_PAD * s, baseline_in(rect[1], rect[3], UI_CHORD * s)],
            max_width: w,
            color: if hovered { colors.text_active } else { colors.text_inactive },
            clip: no_clip,
            px: UI_CHORD * s,
            bold: false,
            tracking: 0.0,
        });
        rect[0] - BAR_PAD * s
    };

    // The active tab's identity: title, `host · cwd`, and the host chip.
    // Reads from the ACTIVE tab — literal text here contradicts the pane the
    // moment the active tab is not the first one. Every run budgets against
    // where the controls start (#51: text laid out at natural width drew
    // straight under them at narrow widths), and the chip yields last — it
    // is the "which machine" fact this UI exists for.
    let reserve =
        model.controls.native_leading.map_or(SLIM_PAD * s, |t| t[0] + TRAFFIC_PAD * s);
    if let Some(tab) = model.tabs.get(model.active) {
        let mut x = reserve;
        let budget_right = controls_left - SLIM_PAD * s;
        let chip_px = UI_STATUS * s;
        let chip_text_w = measure(&tab.host, chip_px, false, 0.0);
        let chip_w = 5.0 * s + 6.0 * s + chip_text_w + 16.0 * s;

        let name_px = 13.0 * s;
        let name_w = measure(&tab.title, name_px, false, 0.0)
            .min((budget_right - x - chip_w - 20.0 * s).max(0.0));
        out.texts.push(TextRun {
            text: tab.title.clone(),
            pos: [x, baseline_in(0.0, header_h, name_px)],
            max_width: name_w,
            color: colors.text_active,
            clip: no_clip,
            px: name_px,
            bold: false,
            tracking: 0.0,
        });
        // The spacer only exists when a title was actually drawn: at widths
        // too tight for the name, charging 10px anyway could push the host
        // chip out even though it fits flush at the reserve edge — and the
        // chip is the identity that yields LAST here, not first.
        if name_w > 0.0 {
            x += name_w + 10.0 * s;
        }

        let detail = tab.detail();
        let detail_w = measure(&detail, UI_SMALL * s, false, 0.0)
            .min((budget_right - x - chip_w - 10.0 * s).max(0.0));
        if detail_w > 8.0 * s {
            out.texts.push(TextRun {
                text: detail,
                pos: [x, baseline_in(0.0, header_h, UI_SMALL * s)],
                max_width: detail_w,
                color: colors.text_inactive,
                clip: no_clip,
                px: UI_SMALL * s,
                bold: false,
                tracking: 0.0,
            });
            x += detail_w + 10.0 * s;
        }

        // The host chip: where this shell runs, said in a pill. An app tab
        // has no host — a pill with an empty label is chrome lint.
        if !tab.host.is_empty() && x + chip_w <= budget_right {
            let chip_h = 20.0 * s;
            let chip = [x, (header_h - chip_h) / 2.0, chip_w, chip_h];
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
                pos: [
                    chip[0] + 8.0 * s + 5.0 * s + 6.0 * s,
                    baseline_in(chip[1], chip_h, chip_px),
                ],
                max_width: chip_text_w + 2.0,
                color: colors.accent,
                clip: no_clip,
                px: chip_px,
                bold: false,
                tracking: 0.0,
            });
        }
    }

    // The sidebar, below the header, full remaining height.
    let sw = m.sidebar_width * s;
    let sidebar = [0.0, header_h, sw, (m.height - header_h).max(0.0)];
    out.rects.push(RectInstance::filled(sidebar, colors.strip_bg, no_clip));
    out.rects.push(RectInstance::filled(
        [sw - HAIRLINE * s, header_h, HAIRLINE * s, sidebar[3]],
        colors.line,
        no_clip,
    ));
    out.hit.push(sidebar, HitRegion::Strip);

    // Search row at the sidebar's top: the search pill with the new-tab `+`
    // to its right — searching and starting a session are the two things you
    // do at the top of a sidebar (design §2, invariant 5). The `+` here is
    // the same launcher the horizontal strip carries.
    let plus_w = NEW_TAB_W * s;
    let search = [
        SEARCH_PAD * s,
        header_h + SEARCH_PAD * s,
        (sw - 2.0 * SEARCH_PAD * s - plus_w - PILL_GAP * s).max(0.0),
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
    {
        let nt = [search[0] + search[2] + PILL_GAP * s, search[1], plus_w, search[3]];
        out.new_tab_rect = nt;
        // Open state on the button, exactly as the horizontal strip does it.
        let open = model.launcher.is_some();
        if open || model.hover == Some(HitRegion::NewTab) {
            out.rects.push(RectInstance::rounded(nt, PILL_RADIUS * s, colors.tab_hover_bg, no_clip));
        }
        out.hit.push(nt, HitRegion::NewTab);
        let glyph_w = measure("+", 16.0 * s, false, 0.0);
        out.texts.push(TextRun {
            text: "+".into(),
            pos: [nt[0] + (nt[2] - glyph_w) / 2.0, baseline_in(nt[1], nt[3], 16.0 * s)],
            max_width: nt[2],
            color: if open { colors.accent } else { colors.text_inactive },
            clip: no_clip,
            px: 16.0 * s,
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
            // App tabs are places, not sessions: they have no host to group
            // under — Settings gets the pinned row below instead.
            if t.kind != TabKind::Session {
                continue;
            }
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

    // The app tabs (§11/§12) are ordinary rows at the end of the list, not a
    // band pinned above the footer: they scroll with everything else, which
    // is what "treat them as normal tabs" costs and buys (#494). Ungrouped —
    // they have no host — so they follow the last group rather than joining
    // one.
    let app_rows: Vec<usize> =
        model.tabs.iter().enumerate()
            .filter(|(_, t)| t.kind != TabKind::Session)
            .map(|(i, _)| i)
            .collect();
    let content_h: f32 = groups
        .iter()
        .map(|g| group_header_h + g.tabs.len() as f32 * row_h + GROUP_GAP * s)
        .sum::<f32>()
        + app_rows.len() as f32 * APP_ROW_H * s;
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
            // The row and its × reveal together and stay revealed together.
            // Testing `Tab` alone would hide the × the moment the pointer
            // entered it, and the click would then fall back through to the
            // row and *activate* the tab instead of closing it.
            let close_hovered = model.hover == Some(HitRegion::TabClose(tab.addr));
            let hovered = close_hovered || model.hover == Some(HitRegion::Tab(tab.addr));
            if active {
                out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.accent_soft, rows_clip));
            } else if hovered {
                out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.tab_hover_bg, rows_clip));
            }
            if let Some(hit) = intersect(row, rows_clip) {
                out.hit.push(hit, HitRegion::Tab(tab.addr));
            }

            // 5px state dot: attention first, then running (pulsing warn on
            // the clock's 1.6s ease), the live session success, an idle one
            // faint. Attention out-ranks the rest because it is the one state
            // that is asking for you — a session both running and ringing
            // has already said which of the two it wants you to act on.
            //
            // The sidebar recolours where the horizontal chip adds a badge:
            // the row has one dot and no glyph tile to hang a corner off, and
            // it carries no `LinkKind` ink to collide with.
            let dot_d = 5.0 * s;
            let dot_color = if tab.attention.is_some() {
                colors.info
            } else if matches!(
                tab.progress,
                zest_core::Progress::At { state: zest_core::ProgressState::Error, .. }
            ) {
                // A job that says it failed says so here, ahead of "running":
                // the block index may not know it ended, and between the two
                // the program's own word about itself is the newer one.
                colors.danger
            } else if tab.running || tab.progress.is_busy() {
                let p = model.anim.pulse;
                LinearRgba([
                    colors.warn.0[0] * p,
                    colors.warn.0[1] * p,
                    colors.warn.0[2] * p,
                    colors.warn.0[3] * p,
                ])
            } else if active {
                colors.success
            } else {
                colors.text_faint
            };
            dot(&mut out.rects, row[0] + 8.0 * s + dot_d / 2.0, y + row_h / 2.0, dot_d, dot_color, rows_clip);

            let text_x = row[0] + 8.0 * s + dot_d + TAB_INNER_GAP * s;
            let mut text_right = row[0] + row[2] - 8.0 * s;

            // One slot at the row's right edge: the age until the pointer
            // arrives, the × while it is here. Reserved at the wider of the
            // two whichever is showing, so the title and cwd are handed the
            // same budget either way — a row whose text reflowed under the
            // pointer would be worse than no × at all. (The horizontal chip
            // can afford to draw its × always; a 262px row with a two-line
            // label cannot spend the width on both.)
            //
            // Unconditional, including on a row with no age yet: the × can
            // appear on any session row, so a slot that materialised with the
            // pointer would move the title under it. Making the reserve
            // conditional is the tempting fix and it is the bug.
            let age_px = UI_CHORD * s;
            let age_w =
                if tab.age.is_empty() { 0.0 } else { measure(&tab.age, age_px, false, 0.0) };
            let slot_w = age_w.max(CLOSE * s);
            if hovered {
                let close = [
                    text_right - CLOSE * s,
                    y + (row_h - CLOSE * s) / 2.0,
                    CLOSE * s,
                    CLOSE * s,
                ];
                if close_hovered {
                    out.rects.push(RectInstance::rounded(close, 4.0 * s, colors.line, rows_clip));
                }
                // After the row's own region, never before it: `hit()` walks
                // in reverse, so the last one pushed is the one that wins.
                if let Some(hit) = intersect(close, rows_clip) {
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
                    clip: rows_clip,
                    px: UI_BODY * s,
                    bold: false,
                    tracking: 0.0,
                });
            } else if age_w > 0.0 {
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
            }
            text_right -= slot_w + TEXT_PAD * s;

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

    // The app tabs (§11/§12), in the same scrolling list as the sessions and
    // wearing the same row: glyph, label, and a right slot that holds the
    // chord until the pointer arrives and turns it into a ×.
    //
    // The slot is reserved at the wider of the two whichever is showing —
    // the session row's rule (`slot_w` below), and for the same reason: a
    // label that reflowed under the pointer is worse than no × at all.
    for &ti in &app_rows {
        let Some(tab) = model.tabs.get(ti) else { continue };
        let row_box = [ROW_HPAD * s, y, sw - 2.0 * ROW_HPAD * s, APP_ROW_H * s];
        let row = [row_box[0], row_box[1] + 1.0 * s, row_box[2], row_box[3] - 2.0 * s];

        let active = ti == model.active;
        // Row and × reveal together and stay revealed together — testing
        // `Tab` alone would hide the × the moment the pointer entered it,
        // and the click would fall through to the row and *activate*.
        let close_hovered = model.hover == Some(HitRegion::TabClose(tab.addr));
        let hovered = close_hovered || model.hover == Some(HitRegion::Tab(tab.addr));
        if active {
            out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.accent_soft, rows_clip));
        } else if hovered {
            out.rects.push(RectInstance::rounded(row, 8.0 * s, colors.tab_hover_bg, rows_clip));
        }
        if let Some(hit) = intersect(row, rows_clip) {
            out.hit.push(hit, HitRegion::Tab(tab.addr));
        }

        let ink = if active { colors.accent } else { colors.text_inactive };
        let glyph = app_tab_glyph(tab.kind);
        let glyph_px = 12.0 * s;
        out.texts.push(TextRun {
            text: glyph.into(),
            pos: [row[0] + 10.0 * s, baseline_in(row[1], row[3], glyph_px)],
            max_width: 16.0 * s,
            color: ink,
            clip: rows_clip,
            px: glyph_px,
            bold: false,
            tracking: 0.0,
        });

        let chord = match tab.kind {
            TabKind::Profiles => &model.profiles_chord,
            _ => &model.settings_chord,
        };
        let chord_w = measure(chord, UI_CHORD * s, false, 0.0);
        let slot_w = chord_w.max(CLOSE * s);
        let slot_right = row[0] + row[2] - 10.0 * s;
        let text_right = slot_right - slot_w - TAB_INNER_GAP * s;

        let text_x = row[0] + 28.0 * s;
        out.texts.push(TextRun {
            text: tab.title.clone(),
            pos: [text_x, baseline_in(row[1], row[3], UI_BODY * s)],
            max_width: (text_right - text_x).max(0.0),
            color: ink,
            clip: rows_clip,
            px: UI_BODY * s,
            bold: false,
            tracking: 0.0,
        });

        if hovered {
            let close = [
                slot_right - CLOSE * s,
                row[1] + (row[3] - CLOSE * s) / 2.0,
                CLOSE * s,
                CLOSE * s,
            ];
            if close_hovered {
                out.rects.push(RectInstance::rounded(close, 4.0 * s, colors.line, rows_clip));
            }
            // After the row's own region, never before it: `hit()` walks in
            // reverse, so the last one pushed is the one that wins.
            if let Some(hit) = intersect(close, rows_clip) {
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
                clip: rows_clip,
                px: UI_BODY * s,
                bold: false,
                tracking: 0.0,
            });
        } else {
            out.texts.push(TextRun {
                text: chord.clone(),
                pos: [slot_right - chord_w, baseline_in(row[1], row[3], UI_CHORD * s)],
                max_width: chord_w + 2.0,
                color: colors.text_faint,
                clip: rows_clip,
                px: UI_CHORD * s,
                bold: false,
                tracking: 0.0,
            });
        }
        y += APP_ROW_H * s;
    }

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

    out
}

/// Baseline for one line of text vertically centred in a band starting at
/// `y` with height `h`.
fn text_baseline(m: &ChromeMetrics, y: f32, h: f32) -> f32 {
    y + (h - m.line_height).max(0.0) / 2.0 + m.baseline
}

#[cfg(test)]
mod tests {
    use super::super::model::{TabModel, TabOrigin, WindowControls};
    use super::*;
    use zest_proto::{HostId, SessionAddr, SessionId};

    fn addr(n: u8) -> SessionAddr {
        SessionAddr::new(HostId::from_bytes([n; 32]), SessionId(u64::from(n)))
    }

    /// A remote origin whose id matches `addr(n)`'s, the way the app builds
    /// them; layout never reads the id, but the fixture should not lie.
    fn remote(n: u8, label: &str) -> TabOrigin {
        TabOrigin::Remote { host: HostId::from_bytes([n; 32]), label: label.to_string() }
    }

    fn tab(n: u8, origin: TabOrigin, presence: TabPresence) -> TabModel {
        // The detail line carries the host's name exactly as the app composes
        // it, so the words-not-colour tests exercise the real shape.
        let host = match &origin {
            TabOrigin::Remote { label, .. } => label.clone(),
            TabOrigin::Local => "local".into(),
        };
        TabModel {
            addr: addr(n),
            kind: TabKind::Session,
            title: format!("tab {n}"),
            host,
            cwd: format!("~/dir{n}"),
            origin,
            presence,
            accent: usize::from(n),
            // What the builder computes for an identity-less tab: the chip
            // shows its host.
            tab_accent: AccentChoice::Host(usize::from(n)),
            running: false,
            progress: zest_core::Progress::None,
            attention: None,
            age: "2m".into(),
            connecting: false,
            link: LinkKind::Loopback,
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
            cell_w: 8.0,
            padding: 0,
        }
    }

    fn model(tabs: Vec<TabModel>, position: TabsPosition) -> ChromeModel {
        ChromeModel {
            tabs,
            active: 0,
            position,
            strip_scroll: 0.0,
            ensure_active_visible: false,
            hover: None,
            controls: WindowControls::default(),
            focused: true,
            sidebar: None,
            screen: None,
            panes: None,
            anim: super::super::model::AnimPhase::default(),
            grid_area: [0.0, 46.0, 1200.0, 726.0],
            palette_chord: "⌘K".into(),
            settings_chord: "\u{2318},".into(),
            profiles_chord: "\u{2318}\u{21e7},".into(),
            picker: None,
            palette: None,
            dir_picker: None,
            open_file: None,
            settings: None,
            launcher: None,
            block_menu: None,
            notice: None,
            approval: None,
            confirm_close: None,
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
                tab(2, remote(2, "alien"), TabPresence::Online),
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

    fn block_menu(anchor: [f32; 4]) -> super::super::model::BlockMenuModel {
        use super::super::model::BlockMenuRow as R;
        super::super::model::BlockMenuModel {
            rows: vec![
                R::Action { label: "Fold".into(), chord: String::new(), enabled: true },
                R::Divider,
                R::Action {
                    label: "Copy output".into(),
                    chord: "\u{2318}\u{21e7}O".into(),
                    enabled: true,
                },
                R::Action { label: "Re-run".into(), chord: String::new(), enabled: false },
            ],
            selected: 0,
            anchor,
        }
    }

    #[test]
    fn the_block_menu_is_in_the_cached_layout_so_it_outranks_the_headers() {
        // What makes the menu win with no ranking code anywhere: `chrome_hit`
        // consults *this* map first and the per-frame block hits only
        // `.or_else(…)`, and the scrim covers the whole window. So while the
        // menu is open no pointer event can reach a header, a rail or the grid.
        let m = metrics(1200.0, 800.0, 1.0);
        let mut model = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        model.block_menu = Some(block_menu([600.0, 300.0, 16.0, 16.0]));
        let l = layout(&model, &colors(), &m, &mut measure);

        assert_eq!(
            l.hit.hit(20.0, 780.0),
            Some(HitRegion::BlockMenuScrim),
            "a far corner is the scrim's, so click-away dismisses"
        );
        let rows: Vec<usize> = (0..1200)
            .step_by(4)
            .flat_map(|x| (0..800).step_by(4).map(move |y| (x, y)))
            .filter_map(|(x, y)| match l.hit.hit(x as f32, y as f32) {
                Some(HitRegion::BlockMenuRow(i)) => Some(i),
                _ => None,
            })
            .collect();
        assert!(rows.contains(&0) && rows.contains(&2), "both live rows answer");
        assert!(
            !rows.contains(&1) && !rows.contains(&3),
            "the divider and the faint row take no clicks, or they answer by doing nothing"
        );
        // And in the *overlay* layer specifically: base chrome draws its text
        // after every base rect, so a panel emitted below the boundary has the
        // block headers' own labels bleeding through it.
        let panel_at = l
            .rects
            .iter()
            .rposition(|r| r.shadow_blur > 0.0)
            .expect("the menu panel casts a shadow");
        assert!(
            panel_at >= l.overlay_rects_at,
            "panel at {panel_at} must sit at or past the overlay boundary {}",
            l.overlay_rects_at
        );
    }

    #[test]
    fn the_block_menu_flips_above_an_anchor_near_the_bottom() {
        // A block's ⋯ is a grid row, so it is near the bottom of the pane far
        // more often than a settings dropdown ever is. Below it the panel
        // would draw off the pane, where its rows are unclickable and the menu
        // looks like it simply ends.
        let m = metrics(1200.0, 800.0, 1.0);
        let mut model = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        let area = model.grid_area;
        let anchor = [600.0, area[1] + area[3] - 24.0, 16.0, 16.0];
        model.block_menu = Some(block_menu(anchor));
        let l = layout(&model, &colors(), &m, &mut measure);

        let top = (0..800)
            .find(|y| matches!(l.hit.hit(600.0, *y as f32), Some(HitRegion::BlockMenuPanel)))
            .expect("the panel is somewhere on that column");
        assert!(
            (top as f32) < anchor[1],
            "the panel must open upward from an anchor at the pane's foot"
        );
        assert!((top as f32) >= area[1], "and stay inside the pane");
    }

    #[test]
    fn the_unfocused_panes_frame_does_not_hand_the_wheel_to_the_strip() {
        // The second instance of #256, and the one nobody reported: `Pane` is
        // pushed over the *whole frame* of the unfocused pane, so a wheel in
        // the middle of a perfectly ordinary terminal scrolled the tab strip.
        let m = metrics(1200.0, 800.0, 1.0);
        let mut model = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        let pane = |focused| super::super::model::PaneModel {
            kind: super::super::model::PaneKind::Session,
            host: "local".into(),
            sub: "~/dev".into(),
            focused,
            accent: 0,
        };
        model.panes = Some(vec![pane(true), pane(false)]);
        let l = layout(&model, &colors(), &m, &mut measure);

        // Deep inside the right (unfocused) pane's body, well clear of both
        // its header and the frame's border.
        let right = pane_frames(model.grid_area, m.scale, 2)[1];
        let body = pane_body(right, m.scale, 8);
        let (x, y) = (body[0] + body[2] / 2.0, body[1] + body[3] / 2.0);
        assert_eq!(
            l.hit.hit(x, y),
            Some(HitRegion::Pane(1)),
            "the unfocused pane claims its whole frame — that is the click-to-focus target"
        );
        assert_ne!(
            super::super::hit::wheel_target(l.hit.hit(x, y), Some(0)),
            super::super::hit::WheelTarget::Strip,
            "a wheel in the middle of a terminal must never scroll the tab strip"
        );
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
    fn every_tab_offers_its_close_affordance() {
        // The design draws the × on every chip (mock, screen 1), not only
        // the active one — a mis-click on a background tab's × should close
        // *that* tab, which the hit map already guarantees by construction.
        let tabs = vec![
            tab(1, TabOrigin::Local, TabPresence::Online),
            tab(2, TabOrigin::Local, TabPresence::Online),
        ];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        let closes: std::collections::HashSet<_> = (0..1200)
            .flat_map(|x| (0..46).map(move |y| (x, y)))
            .filter_map(|(x, y)| match l.hit.hit(x as f32, y as f32) {
                Some(HitRegion::TabClose(a)) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(
            closes,
            [addr(1), addr(2)].into(),
            "each tab's × answers as that tab's close"
        );
    }

    /// Every `TabClose` region the sidebar emits, as a set of addresses.
    fn sidebar_closes(l: &ChromeLayout) -> std::collections::HashSet<SessionAddr> {
        // Stepped by two: the × is 16px square, so nothing this looks for can
        // fall between samples, and the sidebar is a lot of pixels to walk.
        (0..220)
            .step_by(2)
            .flat_map(|x| (0..800).step_by(2).map(move |y| (x, y)))
            .filter_map(|(x, y)| match l.hit.hit(x as f32, y as f32) {
                Some(HitRegion::TabClose(a)) => Some(a),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_sidebar_row_offers_its_close_only_under_the_pointer() {
        // The horizontal chip draws its × always; a 262px row with a two-line
        // label cannot spare the width, so the sidebar swaps the age for it.
        // Resting rows therefore carry no close region at all — which is the
        // half of this that has to be asserted, or a stray region over the
        // age label would close a tab nobody pointed at.
        let tabs = vec![
            tab(1, TabOrigin::Local, TabPresence::Online),
            tab(2, TabOrigin::Local, TabPresence::Online),
        ];
        let m = metrics(1200.0, 800.0, 1.0);
        let resting = layout(&model(tabs.clone(), TabsPosition::Left), &colors(), &m, &mut measure);
        assert!(
            sidebar_closes(&resting).is_empty(),
            "an unpointed sidebar offers no close anywhere"
        );

        let mut mo = model(tabs, TabsPosition::Left);
        mo.hover = Some(HitRegion::Tab(addr(2)));
        let hovered = layout(&mo, &colors(), &m, &mut measure);
        assert_eq!(
            sidebar_closes(&hovered),
            [addr(2)].into(),
            "the pointed row offers its close, and only that row's"
        );
    }

    #[test]
    fn the_sidebar_close_survives_its_own_hover() {
        // The rule that makes the affordance usable at all: the region that
        // reveals the × must also *keep* it revealed. Testing `Tab` alone
        // hides the × the instant the pointer enters it, the hit map loses
        // the region, and the click lands on the row underneath — so the
        // gesture activates the tab it was aimed at closing.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Left);
        mo.hover = Some(HitRegion::TabClose(addr(1)));
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert_eq!(
            sidebar_closes(&l),
            [addr(1)].into(),
            "a × under the pointer is still a ×"
        );
    }

    #[test]
    fn the_sidebar_close_wins_over_its_row() {
        // `TabClose` is pushed after `Tab` and `hit()` walks in reverse.
        // Pushed the other way round the × is decoration: every pixel of it
        // answers as the row, and clicking it activates instead of closes.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Left);
        mo.hover = Some(HitRegion::Tab(addr(1)));
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            !sidebar_closes(&l).is_empty(),
            "the × must out-rank the row it sits inside"
        );
    }

    #[test]
    fn a_sidebar_row_does_not_reflow_when_its_close_appears() {
        // The × takes the age's slot rather than one of its own, and the slot
        // is reserved at the wider of the two whichever is showing. Without
        // that, a title would jump — or worse, re-ellipsise — under the
        // pointer, which reads as the row changing rather than as an
        // affordance appearing.
        let m = metrics(1200.0, 800.0, 1.0);
        let width_of = |age: &str, hover: Option<HitRegion>| {
            let mut t = tab(1, TabOrigin::Local, TabPresence::Online);
            t.age = age.into();
            let mut mo = model(vec![t], TabsPosition::Left);
            mo.hover = hover;
            let l = layout(&mo, &colors(), &m, &mut measure);
            l.texts
                .iter()
                .find(|t| t.text == "tab 1")
                .map(|t| t.max_width)
                .expect("the row draws its title")
        };
        assert_eq!(
            width_of("2m", None),
            width_of("2m", Some(HitRegion::Tab(addr(1)))),
            "the title's budget is the same with the × showing and without it"
        );
        // And with no age at all — a restored tab has none until its session
        // first produces something. The slot is reserved anyway, because the
        // × can appear on any session row and a slot that materialised with
        // the pointer would move the title under it. That the reserve costs a
        // few pixels on a row showing neither is the price of the affordance,
        // and it is *this* assertion rather than a comment because the
        // tempting fix — making the reserve conditional — reintroduces
        // exactly the jump above.
        assert_eq!(
            width_of("", None),
            width_of("", Some(HitRegion::Tab(addr(1)))),
            "a row with no age still does not move when its × appears"
        );
    }

    #[test]
    fn the_sidebars_app_rows_offer_a_close_like_every_other_row() {
        // #494. These rows are laid out by their own code, so the horizontal
        // rule does not cover them — and the sidebar was where the gap was
        // worst: Settings sat pinned above the footer with nothing to point
        // at, and Profiles was not drawn at all.
        for (kind, title) in [(TabKind::Settings, "Settings"), (TabKind::Profiles, "Profiles")] {
            let mut app = tab(2, TabOrigin::Local, TabPresence::Online);
            app.kind = kind;
            app.title = title.into();
            let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online), app];
            let m = metrics(1200.0, 800.0, 1.0);
            let mut mo = model(tabs, TabsPosition::Left);

            let l = layout(&mo, &colors(), &m, &mut measure);
            assert!(
                !sidebar_closes(&l).contains(&addr(2)),
                "{title}: at rest the slot holds the chord, not a ×"
            );

            mo.hover = Some(HitRegion::Tab(addr(2)));
            let l = layout(&mo, &colors(), &m, &mut measure);
            assert!(
                sidebar_closes(&l).contains(&addr(2)),
                "{title}: the pointer on the row reveals a × to point at"
            );
        }
    }

    #[test]
    fn an_app_rows_label_does_not_move_when_its_close_appears() {
        // The session row's rule (`a row with no age still does not move`),
        // applied to the app rows: the right slot is reserved at the wider of
        // the chord and the ×, so the title is handed the same budget either
        // way. Making the reserve conditional is the tempting fix and it is
        // the bug — the label would jump under the pointer.
        let width_of = |hover: Option<HitRegion>| {
            let mut app = tab(2, TabOrigin::Local, TabPresence::Online);
            app.kind = TabKind::Settings;
            app.title = "Settings".into();
            let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online), app], TabsPosition::Left);
            mo.hover = hover;
            let l = layout(&mo, &colors(), &metrics(1200.0, 800.0, 1.0), &mut measure);
            l.texts
                .iter()
                .find(|t| t.text == "Settings")
                .map(|t| t.max_width)
                .expect("the app row carries its label")
        };
        assert_eq!(
            width_of(None),
            width_of(Some(HitRegion::Tab(addr(2)))),
            "the label keeps its budget when the × replaces the chord"
        );
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
    fn the_leading_control_reserve_is_drag_not_tabs() {
        // Tabs drawn under the native buttons would be unclickable pixels;
        // the reserve keeps them out and stays a drag handle.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 2.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.controls.native_leading = Some([140.0, 56.0]);
        let l = layout(&mo, &colors(), &m, &mut measure);
        for x in [5.0, 70.0, 139.0] {
            assert_eq!(
                l.hit.hit(x, 10.0),
                Some(HitRegion::Drag),
                "the button cluster's zone drags the window"
            );
        }
    }

    /// A model with the borderless chrome on, for the caption/resize tests.
    fn borderless(tabs: Vec<TabModel>, position: TabsPosition) -> ChromeModel {
        let mut mo = model(tabs, position);
        mo.controls = WindowControls {
            native_leading: None,
            drawn_caption: true,
            maximized: false,
            resizable_edges: true,
        };
        mo
    }

    /// Sweep the bar at `y` and report each caption button and where it first
    /// answers. Found by sweeping rather than by index, because the point of
    /// the hit map is that a drawn button answers where it was drawn.
    ///
    /// `y` must be below the resize band: the window's top edge outranks
    /// everything, caption buttons included, which is what Windows' own frame
    /// does too.
    fn caption_rects(l: &ChromeLayout, m: &ChromeMetrics, y: f32) -> Vec<(f32, CaptionButton)> {
        let mut found: Vec<(f32, CaptionButton)> = Vec::new();
        let mut x = 0.0;
        while x < m.width {
            if let Some(HitRegion::CaptionButton(b)) = l.hit.hit(x, y) {
                if !found.iter().any(|(_, seen)| *seen == b) {
                    found.push((x, b));
                }
            }
            x += 1.0;
        }
        found
    }

    #[test]
    fn the_caption_cluster_answers_at_every_button_in_both_layouts() {
        // The property the whole hit-map design exists for, applied to the
        // three buttons that close and resize the window.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let m = metrics(1200.0, 800.0, 2.0);
            let mo = borderless(vec![tab(1, TabOrigin::Local, TabPresence::Online)], position);
            let l = layout(&mo, &colors(), &m, &mut measure);
            // Below the resize band, which owns the topmost pixels.
            let found = caption_rects(&l, &m, 30.0);
            assert_eq!(
                found.len(),
                3,
                "{position:?}: all three caption buttons must be hittable, found {found:?}"
            );
            let order: Vec<CaptionButton> = found.iter().map(|(_, b)| *b).collect();
            assert_eq!(
                order,
                vec![CaptionButton::Minimize, CaptionButton::Maximize, CaptionButton::Close],
                "{position:?}: close is rightmost, as it is in every Windows window"
            );
        }
    }

    #[test]
    fn the_caption_cluster_pushes_the_pills_out_of_its_way() {
        // Both are right-aligned, so laying them out independently is exactly
        // how they would come to overlap — and the overlap would be a pill
        // drawn under the close button, i.e. a click that closes the window
        // when the user meant to open the palette.
        let m = metrics(1200.0, 800.0, 2.0);
        let mo = borderless(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        let l = layout(&mo, &colors(), &m, &mut measure);
        let leftmost_caption = caption_rects(&l, &m, 30.0)
            .first()
            .map(|(x, _)| *x)
            .expect("the cluster is drawn");
        let mut x = leftmost_caption;
        while x < m.width {
            assert!(
                !matches!(l.hit.hit(x, 30.0), Some(HitRegion::PalettePill)),
                "the pill reaches into the caption cluster at x={x}"
            );
            x += 1.0;
        }
    }

    #[test]
    fn the_window_edge_outranks_everything_drawn_over_it() {
        // Including a modal scrim: a window must stay resizable while the
        // palette is open, and the hit map's last-pushed-wins rule is the
        // only thing that makes that true.
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo =
            borderless(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.palette = Some(super::super::model::PaletteModel {
            filter: String::new(),
            filter_caret: Default::default(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0.0,
            ensure_visible: false,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert_eq!(l.hit.hit(1.0, 1.0), Some(HitRegion::Resize(ResizeEdge::Nw)));
        assert_eq!(l.hit.hit(m.width - 1.0, m.height / 2.0), Some(HitRegion::Resize(ResizeEdge::E)));
        assert_eq!(l.hit.hit(m.width / 2.0, m.height - 1.0), Some(HitRegion::Resize(ResizeEdge::S)));
    }

    #[test]
    fn the_top_edge_outranks_the_caption_buttons_it_crosses() {
        // Not an accident to be fixed: Windows' own frame reserves its top
        // border for resizing even where it crosses the caption buttons, and
        // a borderless window that did not would be one you cannot resize
        // from the top-right corner at all. The buttons stay comfortably
        // clickable — the band is 5 logical px of a 44px bar.
        let m = metrics(1200.0, 800.0, 1.0);
        let mo = borderless(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        let l = layout(&mo, &colors(), &m, &mut measure);
        let close_x = caption_rects(&l, &m, 30.0)
            .into_iter()
            .find(|(_, b)| *b == CaptionButton::Close)
            .map(|(x, _)| x)
            .expect("close is drawn");
        assert!(
            matches!(l.hit.hit(close_x + 2.0, 1.0), Some(HitRegion::Resize(_))),
            "the top band resizes even over the close button"
        );
        assert_eq!(
            l.hit.hit(close_x + 2.0, 30.0),
            Some(HitRegion::CaptionButton(CaptionButton::Close)),
            "…and two thirds of the way down it is the button again"
        );
    }

    #[test]
    fn corners_beat_edges() {
        // The corner band is where people actually aim to resize both axes;
        // an edge winning there makes the diagonal drag unreachable.
        let m = metrics(1200.0, 800.0, 1.0);
        let mo = borderless(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert_eq!(l.hit.hit(7.0, 7.0), Some(HitRegion::Resize(ResizeEdge::Nw)));
        assert_eq!(l.hit.hit(m.width - 7.0, m.height - 7.0), Some(HitRegion::Resize(ResizeEdge::Se)));
    }

    #[test]
    fn a_maximized_window_has_no_resize_edges() {
        // Resizing from the edge of a maximized window would un-maximize it
        // under the pointer, which is not what the drag meant.
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo =
            borderless(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.controls.resizable_edges = false;
        mo.controls.maximized = true;
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            !matches!(l.hit.hit(1.0, 1.0), Some(HitRegion::Resize(_))),
            "a maximized window offers no edge to drag"
        );
    }

    #[test]
    fn the_native_frame_draws_no_caption_of_its_own() {
        // The default everywhere but Windows, and what `custom_chrome = off`
        // must still produce: the OS owns the buttons and the edges.
        let m = metrics(1200.0, 800.0, 1.0);
        let mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(caption_rects(&l, &m, 30.0).is_empty(), "no caption buttons without custom chrome");
        assert!(
            !matches!(l.hit.hit(1.0, 1.0), Some(HitRegion::Resize(_))),
            "and no resize bands: DefWindowProc still answers for the frame"
        );
    }

    #[test]
    fn the_sidebar_header_drags_and_rows_start_below_it() {
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Left);
        mo.controls.native_leading = Some([70.0, 40.0]);
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
        // Colour is not enough on its own (#23). Unreachability must appear
        // as text in both orientations; the host's *name* now lives where
        // there is room for it — the vertical sidebar and header — not on a
        // 34px chip (design §1: title only).
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let tabs = vec![tab(
                2,
                remote(2, "alien"),
                TabPresence::Unreachable,
            )];
            let m = metrics(1200.0, 800.0, 1.0);
            let l = layout(&model(tabs, position), &colors(), &m, &mut measure);
            assert!(
                l.texts.iter().any(|t| t.text.contains("unreachable")),
                "{position:?} must say unreachability in words"
            );
            if position == TabsPosition::Left {
                assert!(
                    l.texts.iter().any(|t| t.text.to_lowercase().contains("alien")),
                    "the sidebar must spell out the host's name"
                );
            }
        }
    }

    #[test]
    fn the_title_bar_keeps_only_the_palette_pill_at_its_right_end() {
        // Design §1: the layout toggle is gone from the chrome —
        // `tabs.position` is a setting, and a button duplicating a setting
        // is a second source of truth. ⌘K stays, and stays clickable.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        let found = (0..1200)
            .step_by(2)
            .any(|x| (0..46).step_by(2).any(|y| {
                l.hit.hit(x as f32, y as f32) == Some(HitRegion::PalettePill)
            }));
        assert!(found, "the palette pill must be clickable in the strip");
        assert!(
            !l.texts.iter().any(|t| t.text == "Vertical" || t.text == "Horizontal"),
            "no layout-toggle pill is drawn"
        );
    }

    #[test]
    fn nothing_reserves_the_bottom_edge() {
        // The status bar is deleted (design §1: "no status bar") — the
        // window's bottom edge belongs to the grid again, in both layouts.
        // A chrome region still answering there means insets and drawing
        // disagree about who owns the last rows.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
            let m = metrics(1200.0, 800.0, 1.0);
            let l = layout(&model(tabs, position), &colors(), &m, &mut measure);
            // x=600 is right of the sidebar, below the header: pane country.
            assert_eq!(
                l.hit.hit(600.0, 799.0),
                None,
                "{position:?}: the bottom edge is the grid's, not chrome"
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
    fn a_split_tab_offers_the_other_pane_and_keeps_the_focused_grid() {
        // Design screen 5: clicking anywhere on the unfocused pane moves the
        // keyboard; the focused pane's body must stay the grid's — only its
        // header answers as chrome.
        use crate::chrome::model::PaneModel;
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.panes = Some(vec![
            PaneModel { host: "studio".into(), sub: "~/dev".into(), focused: true, accent: 0, kind: super::super::model::PaneKind::Session },
            PaneModel { host: "forge".into(), sub: "C:\\src".into(), focused: false, accent: 2, kind: super::super::model::PaneKind::Session },
        ]);
        let l = layout(&mo, &colors(), &m, &mut measure);

        let area = mo.grid_area;
        let frames = pane_frames(area, 1.0, 2);
        let (lf, rf) = (frames[0], frames[1]);
        let lb = pane_body(lf, 1.0, 8);
        // Middle of the unfocused (right) pane: chrome, and it says which.
        assert_eq!(
            l.hit.hit(rf[0] + rf[2] / 2.0, rf[1] + rf[3] / 2.0),
            Some(HitRegion::Pane(1)),
            "the unfocused pane is one click from the keyboard"
        );
        // Middle of the focused pane's body: nobody's chrome — the grid's.
        assert_eq!(
            l.hit.hit(lb[0] + lb[2] / 2.0, lb[1] + lb[3] / 2.0),
            None,
            "the focused pane's body belongs to the terminal"
        );
        // Its header still answers, so focus can bounce back by header too.
        assert_eq!(
            l.hit.hit(lf[0] + lf[2] / 2.0, lf[1] + 14.0),
            Some(HitRegion::Pane(0)),
            "the focused header is still chrome"
        );
        assert!(
            l.texts.iter().any(|t| t.text == "focused"),
            "the focused pane says so in words"
        );
    }

    #[test]
    fn any_number_of_panes_tile_the_grid_area_and_each_is_a_click_target() {
        // #436: two was the design's shape, not a limit. Every pane gets an
        // equal column, the frames never overlap or leave the area, and each
        // unfocused one is reachable by a click that names its index.
        use crate::chrome::model::PaneModel;
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1600.0, 800.0, 1.0);
        for n in 1..=5 {
            let mut mo = model(tabs.clone(), TabsPosition::Top);
            mo.panes = Some(
                (0..n)
                    .map(|i| PaneModel {
                        kind: super::super::model::PaneKind::Session,
                        host: format!("host{i}"),
                        sub: String::new(),
                        focused: i == 2 % n,
                        accent: i,
                    })
                    .collect(),
            );
            let l = layout(&mo, &colors(), &m, &mut measure);
            let area = mo.grid_area;
            let frames = pane_frames(area, 1.0, n);
            assert_eq!(frames.len(), n, "one frame per pane");
            for (i, f) in frames.iter().enumerate() {
                assert!(f[0] >= area[0] && f[0] + f[2] <= area[0] + area[2] + 0.01, "pane {i} of {n} stays inside the grid area");
                if i > 0 {
                    let prev = frames[i - 1];
                    assert!(f[0] >= prev[0] + prev[2], "pane {i} of {n} does not overlap its left neighbour");
                }
                assert!((f[2] - frames[0][2]).abs() < 0.01, "pane {i} of {n} is the same width as the first");
                let hit = l.hit.hit(f[0] + f[2] / 2.0, f[1] + f[3] / 2.0);
                if i == 2 % n {
                    assert_eq!(hit, None, "the focused pane's body belongs to the terminal");
                } else {
                    assert_eq!(hit, Some(HitRegion::Pane(i)), "pane {i} of {n} is one click from the keyboard");
                }
            }
        }
    }

    #[test]
    fn too_many_panes_for_the_width_still_stay_inside_the_area() {
        // Unbounded panes make this reachable: forty columns in 200px is
        // more margin than width, and the frames must give up margin rather
        // than walk off the right edge.
        let area = [10.0, 20.0, 200.0, 100.0];
        for n in [1, 7, 40, 200] {
            let frames = pane_frames(area, 2.0, n);
            assert_eq!(frames.len(), n);
            for (i, f) in frames.iter().enumerate() {
                assert!(f[0] >= area[0] - 0.01, "pane {i} of {n} starts inside the area");
                assert!(
                    f[0] + f[2] <= area[0] + area[2] + 0.01,
                    "pane {i} of {n} ends inside the area"
                );
                assert!(f[2] >= 0.0 && f[3] >= 0.0, "pane {i} of {n} has a non-negative size");
            }
        }
    }

    /// A plausible monospace cell, for the pane-geometry tests that have to go
    /// all the way through the letterbox to say anything.
    fn cell_metrics() -> zest_font::CellMetrics {
        zest_font::CellMetrics {
            cell_w: 9,
            cell_h: 19,
            baseline: 15,
            underline_y: 17,
            underline_thickness: 1,
            strikeout_y: 9,
        }
    }

    #[test]
    fn a_pane_leaves_the_block_rail_its_room() {
        // #460. The rail is drawn in the grid layer *outside* the grid rect —
        // painting it inside would shave column 0 off every output row — so it
        // needs free pixels beside the cells and is silently not drawn without
        // them. A pane used to inset by a hairline only, and since a pane's
        // grid is sized `floor(body_w / cell_w)` the letterbox then handed
        // back the body rect exactly: a gutter of 0.0, in every pane, always.
        //
        // Asserted through the same three functions the app calls, in the same
        // order, because the bug was in their composition rather than in any
        // one of them.
        let m = cell_metrics();
        for s in [1.0f32, 1.25, 2.0] {
            for n in [2usize, 3, 5] {
                for padding in [4u32, 8] {
                    let area = [8.0, 46.0, 1600.0, 900.0];
                    for (i, frame) in pane_frames(area, s, n).into_iter().enumerate() {
                        let body = pane_body(frame, s, padding);
                        // Exactly how `resize_split_panes` sizes the pane, so
                        // the letterbox sees the grid the pty is given.
                        let cols = ((body[2] / m.cell_w as f32) as usize).max(2);
                        let rows = ((body[3] / m.cell_h as f32) as usize).max(2);
                        let grid = super::super::insets::letterbox(body, cols, rows, m);
                        let g = pane_gutter(frame, grid, s);
                        assert!(
                            g >= zest_render_wgpu::RAIL_PX,
                            "pane {i} of {n} at scale {s}, padding {padding}: gutter {g} is \
                             under the {}px the rail needs, so no block state is drawn",
                            zest_render_wgpu::RAIL_PX
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_unsplit_window_is_untouched_by_the_pane_geometry() {
        // One grid is the unsplit window, and it must come out of the shared
        // function byte-identical to the bare letterbox: no frame, no pane
        // padding, no margin. Every #44 pixel assertion and the #215 "a grid
        // that fills the pane keeps the exact pane rect" rule sit on this.
        let m = cell_metrics();
        let area = [8.0, 46.0, 1600.0, 900.0];
        for grid in [(177usize, 45usize), (60, 10), (400, 200)] {
            assert_eq!(
                pane_grid_rects(area, 1.25, 8, &[grid], m),
                vec![super::super::insets::letterbox(area, grid.0, grid.1, m)],
                "an unsplit tab must not pay a pane's padding"
            );
        }
        assert!(pane_grid_rects(area, 1.0, 8, &[], m).is_empty(), "no panes, no rectangles");
    }

    #[test]
    fn every_pane_gets_its_own_rectangle_inside_its_own_frame() {
        // What `focused_view_rect` and the render pass both read. The rects
        // must stay inside their frames and never overlap, or a header is
        // drawn over the neighbour's output.
        let m = cell_metrics();
        let area = [8.0, 46.0, 1600.0, 900.0];
        let s = 1.25;
        for n in [2usize, 3, 5] {
            let frames = pane_frames(area, s, n);
            let grids: Vec<(usize, usize)> = frames
                .iter()
                .map(|f| {
                    let b = pane_body(*f, s, 8);
                    (((b[2] / m.cell_w as f32) as usize).max(2), ((b[3] / m.cell_h as f32) as usize).max(2))
                })
                .collect();
            let rects = pane_grid_rects(area, s, 8, &grids, m);
            assert_eq!(rects.len(), n, "one rectangle per pane");
            for (i, r) in rects.iter().enumerate() {
                let f = frames[i];
                assert!(
                    r[0] >= f[0] && r[0] + r[2] <= f[0] + f[2] + 0.01,
                    "pane {i} of {n}: the grid must stay inside its own frame"
                );
                if i > 0 {
                    let prev = rects[i - 1];
                    assert!(
                        r[0] >= prev[0] + prev[2],
                        "pane {i} of {n} overlaps its left neighbour's grid"
                    );
                }
            }
        }
    }

    #[test]
    fn a_pane_with_no_padding_leaves_no_room() {
        // The rule is `window.padding`, not a constant of the pane's own: a
        // user who sets it to zero to win back columns must lose the rail in
        // both layouts together, since the unsplit path has nowhere to draw it
        // either. Pinned so it cannot be quietly turned into a fixed inset.
        let m = cell_metrics();
        let frame = pane_frames([8.0, 46.0, 1600.0, 900.0], 1.0, 2)[0];
        let body = pane_body(frame, 1.0, 0);
        let cols = ((body[2] / m.cell_w as f32) as usize).max(2);
        let rows = ((body[3] / m.cell_h as f32) as usize).max(2);
        let grid = super::super::insets::letterbox(body, cols, rows, m);
        assert_eq!(
            pane_gutter(frame, grid, 1.0),
            0.0,
            "no padding is no gutter, exactly as it is for an unsplit window"
        );
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
            filter_caret: Default::default(),
            scroll: 0.0,
            ensure_visible: false,
            hosts_searched: 4,
            caret_on: true,
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

    #[test]
    fn the_dir_picker_is_modal_and_every_row_answers_as_itself() {
        // The chrome discipline, applied to the browser (#439): every point
        // in the window answers as the picker's rows, panel or scrim, and a
        // click on row i's centre must return row i — the drawn rect and
        // the hit rect come from one computation.
        use crate::chrome::model::DirPickerModel;
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.dir_picker = Some(DirPickerModel {
            rows: vec![
                ".. (parent directory)".into(),
                "clients".into(),
                "crates".into(),
                "docs".into(),
            ],
            has_parent: true,
            selected: 1,
            filter: String::new(),
            filter_caret: Default::default(),
            scroll: 0.0,
            ensure_visible: false,
            loading: false,
            error: String::new(),
            truncated: false,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);

        let mut seen_rows = std::collections::HashSet::new();
        let mut off_overlay = 0u32;
        for x in (0..1200).step_by(4) {
            for y in (0..800).step_by(4) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::DirPickerRow(i)) => {
                        seen_rows.insert(i);
                    }
                    Some(HitRegion::DirPickerPanel | HitRegion::DirPickerScrim) => {}
                    other => {
                        off_overlay += 1;
                        assert!(
                            other.is_none(),
                            "a point answered as {other:?} under a modal overlay"
                        );
                    }
                }
            }
        }
        assert_eq!(off_overlay, 0, "the scrim must cover every last pixel");
        assert_eq!(
            seen_rows,
            (0..4).collect::<std::collections::HashSet<_>>(),
            "all four rows are clickable, the parent row included"
        );
    }

    #[test]
    fn a_truncated_listing_reserves_its_footer_instead_of_sitting_on_a_row() {
        // The note has to go somewhere the rows are not: the panel's padding
        // is a third of a row tall, so a footer drawn into the listing's own
        // space lands on the last directory. Asserted through the hit map,
        // which is the only thing that knows where rows actually ended up.
        use crate::chrome::model::DirPickerModel;
        let m = metrics(1200.0, 800.0, 1.0);
        let rows: Vec<String> = (0..6).map(|i| format!("dir-{i}")).collect();

        let bottom_gap = |truncated: bool| {
            let mut mo = model(
                vec![tab(1, TabOrigin::Local, TabPresence::Online)],
                TabsPosition::Top,
            );
            mo.dir_picker = Some(DirPickerModel {
                rows: rows.clone(),
                has_parent: false,
                selected: 0,
                filter: String::new(),
                filter_caret: Default::default(),
                scroll: 0.0,
                ensure_visible: false,
                loading: false,
                error: String::new(),
                truncated,
            });
            let l = layout(&mo, &colors(), &m, &mut measure);
            let (mut last_row_y, mut panel_bottom) = (0.0f32, 0.0f32);
            for y in (0..800).step_by(2) {
                match l.hit.hit(600.0, y as f32) {
                    Some(HitRegion::DirPickerRow(_)) => last_row_y = y as f32,
                    Some(HitRegion::DirPickerPanel) => panel_bottom = y as f32,
                    _ => {}
                }
            }
            panel_bottom - last_row_y
        };

        let plain = bottom_gap(false);
        let truncated = bottom_gap(true);
        assert!(
            truncated - plain >= PALETTE_ROW_H - 2.0,
            "a truncated listing must reserve a whole row for its note \
             (gap grew by {}, wanted about {PALETTE_ROW_H})",
            truncated - plain
        );
    }

    #[test]
    fn the_dir_picker_scroll_is_clamped_and_ensure_visible_reaches_the_selection() {
        use crate::chrome::model::DirPickerModel;
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.dir_picker = Some(DirPickerModel {
            rows: (0..200).map(|i| format!("dir-{i}")).collect(),
            has_parent: false,
            selected: 199,
            filter: String::new(),
            filter_caret: Default::default(),
            scroll: 0.0,
            ensure_visible: true,
            loading: false,
            error: String::new(),
            truncated: true,
        });
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.dir_picker_scroll > 0.0, "reaching the last of 200 rows means scrolling");
        assert!(l.dir_picker_scroll < 1e9, "and the scroll is clamped to the content");
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
            filter_caret: Default::default(),
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
            filter_caret: Default::default(),
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

    #[test]
    fn the_open_menu_draws_in_the_overlay_layer() {
        // #182: the chrome draws all rects then all texts, so a menu emitted
        // in the base pass has every base text under its footprint painted
        // OVER its opaque panel — the segmented control's labels showed
        // through the backdrop dropdown. Floating panels live past the
        // overlay markers, like the picker and the launcher.
        use crate::chrome::model::{
            SettingsMenuModel, SettingsMenuOption, SettingsRowModel, SettingsValueCell,
        };
        let mk = |menu_open: bool| {
            let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
            let mut mo = model(tabs, TabsPosition::Top);
            mo.grid_area = [0.0, 46.0, 1200.0, 754.0];
            let rows = vec![SettingsRowModel::Setting {
                label: "Backdrop".into(),
                key: "window.backdrop".into(),
                description: "the documented dropdown".into(),
                value: SettingsValueCell::Select { value: "acrylic".into() },
                provenance: None,
                restart: false,
                inert: false,
                modified: false,
            }];
            let mut screen = settings_screen_model(rows, 0, false);
            if menu_open {
                screen.menu = Some(SettingsMenuModel {
                    row: 0,
                    options: vec![SettingsMenuOption {
                        label: "Acrylic".into(),
                        value: "acrylic".into(),
                        doc: "Acrylic blur.".into(),
                    }],
                    current: Some(0),
                    selected: 0,
                    searchable: false,
                    filter: String::new(),
                    filter_caret: Default::default(),
                    scroll: 0.0,
                    ensure_visible: false,
                    footer: None,
                });
            }
            mo.settings = Some(screen);
            mo
        };
        let m = metrics(1200.0, 800.0, 1.0);
        let closed = layout(&mk(false), &colors(), &m, &mut measure);
        assert_eq!(
            closed.rects.len(),
            closed.overlay_rects_at,
            "with no floating panel open, nothing draws in the overlay layer"
        );
        let open = layout(&mk(true), &colors(), &m, &mut measure);
        assert!(
            open.rects.len() > open.overlay_rects_at,
            "the open menu's panel must land after the overlay marker, above every base text"
        );
        assert!(
            open.texts.len() > open.overlay_texts_at,
            "and its labels with it, or the panel covers its own options"
        );
    }

    #[test]
    fn a_text_row_is_an_input_you_can_click() {
        // #183: §11's text widget is a visible 32px input — panel fill,
        // hairline border, click begins the edit. A bare right-aligned value
        // gave the user nothing to aim at and no sign editing exists.
        use crate::chrome::model::{SettingsRowModel, SettingsValueCell};
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.grid_area = [0.0, 46.0, 1200.0, 754.0];
        let rows = vec![SettingsRowModel::Setting {
            label: "Shell".into(),
            key: "shell.command".into(),
            description: "what runs".into(),
            value: SettingsValueCell::Text { text: "pwsh -NoLogo".into(), placeholder: false },
            provenance: None,
            restart: false,
            inert: false,
            modified: false,
        }];
        mo.settings = Some(settings_screen_model(rows, 0, false));
        let l = layout(&mo, &colors(), &m, &mut measure);
        let boxr = (0..1200).step_by(2).find_map(|x| {
            (46..800)
                .step_by(2)
                .find_map(|y| {
                    (l.hit.hit(x as f32, y as f32) == Some(HitRegion::SettingsSelect(0)))
                        .then_some(())
                })
                .map(|()| x)
        });
        assert!(
            boxr.is_some(),
            "the text value must be a hittable input box that begins the edit on click"
        );
    }

    fn settings_rows(n: usize) -> Vec<crate::chrome::model::SettingsRowModel> {
        use crate::chrome::model::{SettingsRowModel, SettingsValueCell};
        (0..n)
            .map(|i| SettingsRowModel::Setting {
                label: format!("Setting {i}"),
                key: format!("group.key_{i}"),
                description: "a setting".into(),
                value: SettingsValueCell::Toggle { on: i % 2 == 0 },
                provenance: None,
                restart: false,
                inert: false,
                modified: false,
            })
            .collect()
    }

    fn settings_screen_model(
        rows: Vec<crate::chrome::model::SettingsRowModel>,
        selected: usize,
        ensure_visible: bool,
    ) -> crate::chrome::model::SettingsScreenModel {
        use crate::chrome::model::{SettingsCategoryModel, SettingsScreenModel};
        SettingsScreenModel {
            categories: vec![SettingsCategoryModel { label: "Text".into(), modified: 0 }],
            selected_category: 0,
            heading: "Text".into(),
            prefix: "typography".into(),
            lede: "Fonts and cells.".into(),
            rows,
            empty: None,
            selected,
            filter: String::new(),
            filter_caret: Default::default(),
            scroll: 0.0,
            ensure_visible,
            modified_total: 0,
            config_path: "~/.config/zesterm/config.toml".into(),
            menu: None,
        }
    }

    #[test]
    fn the_settings_tab_swallows_the_grid_area_beneath_it() {
        // Not a modal — no scrim, the strip stays live — but inside the grid
        // area every point must resolve to a settings region: the session
        // underneath must not take clicks through its own settings screen.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.grid_area = [0.0, 46.0, 1200.0, 754.0];
        mo.settings = Some(settings_screen_model(settings_rows(3), 1, false));
        let l = layout(&mo, &colors(), &m, &mut measure);

        let mut seen_rows = std::collections::HashSet::new();
        for x in (0..1200).step_by(4) {
            for y in (50..800).step_by(4) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::SettingsRow(i) | HitRegion::SettingsToggle(i)) => {
                        seen_rows.insert(i);
                    }
                    Some(
                        HitRegion::SettingsPanel
                        | HitRegion::SettingsCategory(_)
                        | HitRegion::SettingsFilter
                        | HitRegion::SettingsReset(_)
                        | HitRegion::SettingsEditToml
                        | HitRegion::Resize(_),
                    ) => {}
                    Some(other) => {
                        panic!("a click at ({x},{y}) fell through the settings tab: {other:?}")
                    }
                    None => panic!("({x},{y}) hit nothing inside the settings tab"),
                }
            }
        }
        assert_eq!(
            seen_rows,
            [0usize, 1, 2].into(),
            "every setting row must be clickable"
        );
    }

    #[test]
    fn keyboard_navigation_never_acts_on_an_offscreen_row() {
        // Forty rows overflow the column. With the selection at the end and
        // the scroll at the top, ensure_visible must move the scroll so the
        // selected row is actually hittable — otherwise arrows act on rows
        // the user cannot see.
        let m = metrics(1200.0, 800.0, 1.0);
        let rows = settings_rows(40);
        let selected = rows.len() - 1;
        let mut mo = model(Vec::new(), TabsPosition::Top);
        mo.grid_area = [0.0, 46.0, 1200.0, 754.0];
        mo.settings = Some(settings_screen_model(rows, selected, true));
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
        use crate::chrome::model::{SettingsRowModel, SettingsValueCell};
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(Vec::new(), TabsPosition::Top);
        mo.grid_area = [0.0, 46.0, 1200.0, 754.0];
        mo.settings = Some(settings_screen_model(
            vec![SettingsRowModel::Setting {
                label: "Opacity".into(),
                key: "window.opacity".into(),
                description: "background opacity".into(),
                value: SettingsValueCell::Slider { frac: 0.5, text: "0.5".into() },
                provenance: None,
                restart: false,
                inert: false,
                modified: false,
            }],
            0,
            false,
        ));
        let l = layout(&mo, &colors(), &m, &mut measure);
        let (row, track) =
            *l.settings_tracks.first().expect("the slider row must report its track");
        assert_eq!(row, 0);
        // The centre of the reported track must answer as that slider.
        let (cx, cy) = (track[0] + track[2] / 2.0, track[1] + track[3] / 2.0);
        assert_eq!(
            l.hit.hit(cx, cy),
            Some(HitRegion::SettingsSlider(0)),
            "the grab band must cover the track it reports"
        );
    }

    #[test]
    fn the_vertical_header_text_yields_to_the_right_controls() {
        // #51: at narrow widths the header's right-hand controls drew straight
        // over the session path and the host chip. The header's text must
        // budget against wherever the controls start, at any width.
        let m = metrics(500.0, 300.0, 1.0);
        let mut tabs = vec![
            tab(1, TabOrigin::Local, TabPresence::Online),
            tab(2, TabOrigin::Local, TabPresence::Online),
        ];
        tabs[1].cwd = "~/dev/zesterm/branches/49-screenshot".into();
        let mut mo = borderless(tabs, TabsPosition::Left);
        mo.active = 1;
        let l = layout(&mo, &colors(), &m, &mut measure);

        // Where the right-hand controls begin, found by sweeping like a
        // pointer would — below the resize band, which owns the top pixels.
        let controls_start = (0..500)
            .map(|x| x as f32)
            .find(|&x| {
                matches!(
                    l.hit.hit(x, 22.0),
                    Some(HitRegion::PalettePill | HitRegion::CaptionButton(_))
                )
            })
            .expect("the header carries controls at its right end");
        for run in l.texts.iter().filter(|t| {
            t.pos[1] < 46.0
                && (t.text.contains("tab 2")
                    || t.text.contains("~/dev/zesterm")
                    || t.text == "local")
        }) {
            assert!(
                run.pos[0] + run.max_width <= controls_start + 0.5,
                "header text {:?} reaches x={} into the controls at x={controls_start}",
                run.text,
                run.pos[0] + run.max_width,
            );
        }
        // And the header reads from the ACTIVE tab — literal text there
        // contradicts the pane the moment the active tab is not the first.
        assert!(
            l.texts.iter().any(|t| t.text.contains("tab 2")),
            "the header names the active session, not the first one"
        );
    }

    #[test]
    fn a_chip_carries_title_only() {
        // Design §1, and the structural fix for #51's class of bug: the
        // 9.5px `host · cwd` sub-line is gone — host and cwd live in the
        // vertical sidebar and header, which have room for them.
        let tabs = vec![
            tab(1, TabOrigin::Local, TabPresence::Online),
            tab(2, remote(2, "alien"), TabPresence::Online),
        ];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        assert!(
            l.texts.iter().all(|t| (t.px - 9.5).abs() > 0.01),
            "no 9.5px sub-line text survives in the horizontal chrome"
        );
        assert!(
            !l.texts.iter().any(|t| t.text.contains("~/dir")),
            "cwd does not appear on a chip; it lives in the tooltip and the sidebar"
        );

        // The remaining line is budgeted: at the 104px floor the title stops
        // before the close affordance begins, never under it.
        let tabs: Vec<_> =
            (1..=8).map(|n| tab(n, TabOrigin::Local, TabPresence::Online)).collect();
        let m = metrics(500.0, 300.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        let close_left = (0..500)
            .map(|x| x as f32)
            .find(|&x| l.hit.hit(x, 21.0) == Some(HitRegion::TabClose(addr(1))))
            .expect("the first chip shows its close affordance");
        let title = l
            .texts
            .iter()
            .find(|t| t.text == "tab 1")
            .expect("the first chip carries its title");
        assert!(
            title.pos[0] + title.max_width <= close_left,
            "the title's budget ({}) must stop before the close region ({close_left})",
            title.pos[0] + title.max_width
        );
    }

    #[test]
    fn chips_floor_at_104_then_the_strip_scrolls() {
        // Below 104px the label degrades to a single letter, so chips stop
        // shrinking there and the row scrolls instead of crushing (§1).
        let tabs: Vec<_> =
            (1..=10).map(|n| tab(n, TabOrigin::Local, TabPresence::Online)).collect();
        let m = metrics(800.0, 600.0, 1.0);
        let mo = model(tabs, TabsPosition::Top);
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.strip_scroll >= 0.0);
        // Chip pitch = width + gap, read off the hit map like a pointer
        // would: the first x where each chip answers.
        let first_x = |l: &ChromeLayout, n: u8| {
            (0..800).map(|x| x as f32).find(|&x| {
                matches!(
                    l.hit.hit(x, 29.0),
                    Some(HitRegion::Tab(a) | HitRegion::TabClose(a)) if a == addr(n)
                )
            })
        };
        let x1 = first_x(&l, 1).expect("chip 1 visible at scroll 0");
        let x2 = first_x(&l, 2).expect("chip 2 visible at scroll 0");
        assert!(
            ((x2 - x1) - (TAB_MIN + TAB_GAP)).abs() < 0.6,
            "ten chips in an 800px strip sit at the 104px floor, got pitch {}",
            x2 - x1
        );

        // The `+` sits outside the scroll offset: same place at scroll 0 and
        // at a wild scroll, so its future menu can never be clipped or lost.
        let nt_at = |l: &ChromeLayout| {
            (0..800)
                .map(|x| x as f32)
                .find(|&x| l.hit.hit(x, 23.0) == Some(HitRegion::NewTab))
        };
        let at_zero = nt_at(&l).expect("the + is reachable at scroll 0");
        let mut scrolled = model(
            (1..=10).map(|n| tab(n, TabOrigin::Local, TabPresence::Online)).collect(),
            TabsPosition::Top,
        );
        scrolled.strip_scroll = 1e9;
        let l = layout(&scrolled, &colors(), &m, &mut measure);
        assert!(l.strip_scroll > 0.0, "an overflowing strip scrolls");
        let at_max = nt_at(&l).expect("the + is reachable at max scroll");
        assert!(
            (at_zero - at_max).abs() < 0.5,
            "the + must not move with the scroll: {at_zero} vs {at_max}"
        );
    }

    #[test]
    fn chips_prefer_the_168_basis_and_never_grow_past_it() {
        // The mock's `flex: 0 1 168px`: one tab in a wide window takes the
        // basis, not the 232px ceiling — chips shrink, they do not grow.
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
        let xs: Vec<f32> = (0..1200)
            .map(|x| x as f32)
            .filter(|&x| {
                matches!(
                    l.hit.hit(x, 29.0),
                    Some(HitRegion::Tab(_) | HitRegion::TabClose(_))
                )
            })
            .collect();
        let width = xs.last().unwrap() - xs.first().unwrap() + 1.0;
        assert!(
            (width - TAB_BASIS).abs() < 1.5,
            "a lone chip is basis-wide (168), got {width}"
        );
    }

    #[test]
    fn activating_an_offscreen_tab_scrolls_it_into_view() {
        // The picker's ensure-visible discipline, applied to the strip:
        // activation must never land on a chip the user cannot see.
        let tabs: Vec<_> =
            (1..=20).map(|n| tab(n, TabOrigin::Local, TabPresence::Online)).collect();
        let m = metrics(1000.0, 800.0, 1.0);
        let mut mo = model(tabs, TabsPosition::Top);
        mo.active = 19;
        mo.strip_scroll = 0.0;
        let visible = |l: &ChromeLayout| {
            (0..1000).step_by(2).any(|x| {
                matches!(
                    l.hit.hit(x as f32, 29.0),
                    Some(HitRegion::Tab(a) | HitRegion::TabClose(a)) if a == addr(20)
                )
            })
        };
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(!visible(&l), "sanity: the last of twenty tabs starts offscreen");

        mo.ensure_active_visible = true;
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(l.strip_scroll > 0.0, "the strip scrolled to reach the activation");
        assert!(visible(&l), "the active chip must be hittable after ensure-visible");
    }

    #[test]
    fn app_tabs_size_to_content_and_offer_a_close() {
        // Design §11/§12: Settings and Profiles are ordinary tabs with the
        // same active treatment and the same close ×; the one thing they do
        // differently is size to their content instead of taking the 168px
        // basis. The × has to be paid for in that width, or it lands on the
        // label (#494).
        let mut settings = tab(9, TabOrigin::Local, TabPresence::Online);
        settings.kind = TabKind::Settings;
        settings.title = "Settings".into();
        let tabs = vec![
            tab(1, TabOrigin::Local, TabPresence::Online),
            tab(2, TabOrigin::Local, TabPresence::Online),
            settings,
        ];
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);

        let extent = |a: SessionAddr| {
            let xs: Vec<f32> = (0..1200)
                .map(|x| x as f32)
                .filter(|&x| {
                    matches!(
                        l.hit.hit(x, 29.0),
                        Some(HitRegion::Tab(b) | HitRegion::TabClose(b)) if b == a
                    )
                })
                .collect();
            xs.last().copied().unwrap_or(0.0) - xs.first().copied().unwrap_or(0.0) + 1.0
        };
        assert!(
            extent(addr(9)) < extent(addr(1)),
            "the app tab ({}) sizes to its label, narrower than a session chip ({})",
            extent(addr(9)),
            extent(addr(1))
        );
        let close_anywhere = (0..1200).step_by(2).any(|x| {
            (0..46).any(|y| l.hit.hit(x as f32, y as f32) == Some(HitRegion::TabClose(addr(9))))
        });
        assert!(close_anywhere, "an app tab shows the close affordance every chip has");
        assert!(
            l.texts.iter().any(|t| t.text == "Settings"),
            "the app tab carries its label"
        );
    }

    #[test]
    fn the_profiles_tab_is_drawn_in_both_positions() {
        // The regression #494 was opened for. Profiles was gated to
        // `TabsPosition::Top`, so with left tabs ⌘⇧, opened a pane the
        // sidebar could neither show nor close — and a horizontal-only test
        // is exactly the one that cannot see it.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let mut profiles = tab(9, TabOrigin::Local, TabPresence::Online);
            profiles.kind = TabKind::Profiles;
            profiles.title = "Profiles".into();
            profiles.host = String::new();
            let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online), profiles];
            let m = metrics(1000.0, 700.0, 1.0);
            let mut mo = model(tabs, position);
            mo.hover = Some(HitRegion::Tab(addr(9)));
            let l = layout(&mo, &colors(), &m, &mut measure);

            assert!(
                l.texts.iter().any(|t| t.text == "Profiles"),
                "{position:?}: the Profiles tab carries its label"
            );
            let hit_anywhere = |want: HitRegion| {
                (0..1000).step_by(2).any(|x| {
                    (0..700).step_by(2).any(|y| l.hit.hit(x as f32, y as f32) == Some(want))
                })
            };
            assert!(
                hit_anywhere(HitRegion::Tab(addr(9))),
                "{position:?}: …and can be clicked"
            );
            assert!(
                hit_anywhere(HitRegion::TabClose(addr(9))),
                "{position:?}: …and closed by pointing at it"
            );
        }
    }

    #[test]
    fn the_two_app_tabs_wear_different_glyphs() {
        // Both drew ⚙ before #494, which is two chips you have to read the
        // label of to tell apart — the glyph is doing no work at that point.
        let glyphs = |kind| {
            let mut app = tab(9, TabOrigin::Local, TabPresence::Online);
            app.kind = kind;
            app.title = "App".into();
            let l = layout(
                &model(vec![app], TabsPosition::Top),
                &colors(),
                &metrics(800.0, 600.0, 1.0),
                &mut measure,
            );
            l.texts.iter().map(|t| t.text.clone()).collect::<Vec<_>>()
        };
        assert!(glyphs(TabKind::Settings).contains(&"\u{2699}".to_string()), "Settings wears ⚙");
        assert!(glyphs(TabKind::Profiles).contains(&"\u{25a4}".to_string()), "Profiles wears ▤");
    }

    #[test]
    fn the_vertical_sidebar_lists_the_app_tabs_after_the_sessions() {
        // #494: the app rows follow the last host group *in the scrolling
        // list* — they used to be a band pinned above the fleet footer,
        // which is not where an ordinary tab lives. Still ungrouped: they
        // have no host, and a row under a fake one would be worse than none.
        let mut settings = tab(9, TabOrigin::Local, TabPresence::Online);
        settings.kind = TabKind::Settings;
        settings.title = "Settings".into();
        settings.host = String::new();
        let tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online), settings];
        let m = metrics(800.0, 600.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Left), &colors(), &m, &mut measure);

        let rows_of = |a: SessionAddr| -> Vec<f32> {
            (0..600)
                .map(|y| y as f32)
                .filter(|&y| l.hit.hit(30.0, y) == Some(HitRegion::Tab(a)))
                .collect()
        };
        let session = rows_of(addr(1));
        let app = rows_of(addr(9));
        assert!(!app.is_empty(), "the app row answers as the settings tab");
        assert!(
            app[0] > *session.last().expect("the session row is drawn"),
            "the app row follows the sessions: session {session:?}, app {app:?}"
        );
        let footer_top = 600.0 - FOOTER_H;
        assert!(
            app.iter().all(|&y| y < footer_top - 40.0),
            "…and sits among the rows, not pinned against the footer: {app:?}"
        );
        assert!(
            l.texts.iter().any(|t| t.text == "\u{2318},"),
            "the chord rides the app row"
        );
        assert!(
            l.texts.iter().filter(|t| t.text == "Settings").count() == 1,
            "settings appears once — listed, not also grouped under a host"
        );
    }

    #[test]
    fn a_degraded_link_inks_its_chip() {
        // The one fact the deleted status bar owned alone: link degradation.
        // It surfaces on the affected tab's glyph tile — warn when stalled,
        // danger when reconnecting — active or not, or a background tab
        // could sit buffering with no visible sign anywhere.
        let c = colors();
        for (link, ink) in [(LinkKind::Stalled, c.warn), (LinkKind::Reconnecting, c.danger)] {
            let mut tabs = vec![
                tab(1, TabOrigin::Local, TabPresence::Online),
                tab(2, remote(2, "alien"), TabPresence::Online),
            ];
            tabs[1].link = link;
            let m = metrics(1200.0, 800.0, 1.0);
            let l = layout(&model(tabs, TabsPosition::Top), &colors(), &m, &mut measure);
            assert!(
                l.rects.iter().any(|r| r.fill == ink && r.rect[1] < 46.0),
                "{link:?} must ink a rect in the strip with its state colour"
            );
        }
    }

    #[test]
    fn the_approval_modal_owns_the_window_and_its_buttons_answer() {
        // ROADMAP M4's modal: a device is asking to attach and a person at
        // this machine decides. Three properties carry the security of it —
        // the code is drawn (it is the person's entire comparison input),
        // the two buttons answer exactly where they are drawn, and a click
        // anywhere else lands on the modal's own panel/scrim rather than
        // falling through to a grid that would treat it as a selection.
        use super::super::model::ApprovalModel;
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.approval = Some(ApprovalModel {
            label: "andy-phone".into(),
            remote: "192.168.1.42:60123".into(),
            code: "481502".into(),
            expires: "code expires in 2m".into(),
        });
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&mo, &colors(), &m, &mut measure);

        assert!(
            l.texts.iter().any(|t| t.text == "481502"),
            "the matching code must be drawn — it is what the person compares"
        );
        assert!(
            l.texts.iter().any(|t| t.text.contains("andy-phone")
                && t.text.contains("192.168.1.42")),
            "the prompt names who is asking and from where — the decision input"
        );

        let find = |region: HitRegion| -> Option<(f32, f32)> {
            for y in 0..800 {
                for x in (0..1200).step_by(4) {
                    if l.hit.hit(x as f32, y as f32) == Some(region) {
                        return Some((x as f32, y as f32));
                    }
                }
            }
            None
        };
        let approve = find(HitRegion::ApprovalApprove).expect("Approve answers somewhere");
        let deny = find(HitRegion::ApprovalDeny).expect("Deny answers somewhere");
        assert!(
            deny.0 < approve.0,
            "Approve sits rightmost — the affirmative corner every dialog trains"
        );

        // Modal means modal: away from the buttons, the panel/scrim answers,
        // so nothing reaches the grid or the strip beneath (the resize edges
        // at the window's rim are the deliberate exception).
        for (x, y) in [(600.0, 400.0), (30.0, 700.0), (1100.0, 100.0)] {
            assert_eq!(
                l.hit.hit(x, y),
                Some(HitRegion::ApprovalPanel),
                "({x},{y}) must land on the modal, not fall through it"
            );
        }

        // And absent, nothing of it remains.
        mo.approval = None;
        let quiet = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            quiet.texts.iter().all(|t| t.text != "481502"),
            "a resolved request leaves no trace of its code"
        );
    }

    fn confirm(choices: ConfirmChoices) -> super::super::model::ConfirmCloseModel {
        super::super::model::ConfirmCloseModel {
            addr: addr(1),
            title: "Close \u{201c}vim\u{201d}?".into(),
            body: "cargo build --release is still running.".into(),
            hint: "Detaching leaves it running.".into(),
            choices,
        }
    }

    /// The first pixel in the window that answers as `region`, if any.
    fn find_region(l: &ChromeLayout, region: HitRegion) -> Option<(f32, f32)> {
        (0..800).step_by(2).find_map(|y| {
            (0..1200).step_by(4).find_map(|x| {
                (l.hit.hit(x as f32, y as f32) == Some(region)).then_some((x as f32, y as f32))
            })
        })
    }

    #[test]
    fn the_close_confirm_owns_the_window_and_its_answers_are_where_it_says() {
        // #381. Three properties: the question names what it would end (a
        // modal that says "something is running" is one nobody can answer),
        // every button answers exactly where it is drawn, and a click
        // anywhere else lands on the panel rather than falling through to a
        // grid that would start a selection under an open question.
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.confirm_close = Some(confirm(ConfirmChoices::DetachOrClose));
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&mo, &colors(), &m, &mut measure);

        assert!(
            l.texts.iter().any(|t| t.text.contains("cargo build --release")),
            "the modal names the command it would end"
        );
        let close = find_region(&l, HitRegion::ConfirmClose).expect("Close answers somewhere");
        let detach = find_region(&l, HitRegion::ConfirmDetach).expect("Detach answers somewhere");
        let cancel = find_region(&l, HitRegion::ConfirmCancel).expect("Cancel answers somewhere");
        assert!(
            cancel.0 < close.0 && close.0 < detach.0,
            "the corner every dialog trains the hand to reach holds the answer that \
             destroys nothing; Cancel sits leftmost, where a misfire is cheapest \
             (cancel {cancel:?}, close {close:?}, detach {detach:?})"
        );

        for (x, y) in [(600.0, 700.0), (30.0, 120.0), (1100.0, 60.0)] {
            assert_eq!(
                l.hit.hit(x, y),
                Some(HitRegion::ConfirmPanel),
                "({x},{y}) must land on the modal, not fall through it"
            );
        }

        mo.confirm_close = None;
        let quiet = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            find_region(&quiet, HitRegion::ConfirmPanel).is_none(),
            "an answered question leaves nothing behind"
        );
    }

    #[test]
    fn the_confirm_offers_no_detach_when_there_is_nothing_to_detach_from() {
        // An in-process pty has no daemon holding it, so Detach would be a
        // button for an outcome this build cannot produce — worse than one
        // fewer button, because the person would believe the shell survived.
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.confirm_close = Some(confirm(ConfirmChoices::CloseOnly));
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            find_region(&l, HitRegion::ConfirmDetach).is_none(),
            "no Detach button, and no region pretending to be one"
        );
        assert!(find_region(&l, HitRegion::ConfirmClose).is_some(), "the other two remain");
        assert!(find_region(&l, HitRegion::ConfirmCancel).is_some());
    }

    #[test]
    fn a_refusal_offers_nothing_destructive() {
        // ⌘B on a tab with no daemon is answered with a statement, not a
        // question. Putting "Close and stop it" in the corner would leave the
        // one gesture that promised not to end a shell a single click from
        // ending it — and the corner is the position every dialog trains the
        // hand to reach for.
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.confirm_close = Some(super::super::model::ConfirmCloseModel {
            addr: addr(1),
            title: "Cannot detach \u{201c}shell\u{201d}".into(),
            body: String::new(),
            hint: "This tab's shell is owned by this window.".into(),
            choices: ConfirmChoices::Acknowledge,
        });
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&mo, &colors(), &m, &mut measure);
        assert!(find_region(&l, HitRegion::ConfirmClose).is_none(), "nothing here ends a shell");
        assert!(find_region(&l, HitRegion::ConfirmDetach).is_none(), "and nothing pretends to");
        assert!(find_region(&l, HitRegion::ConfirmCancel).is_some(), "one way out, which is out");
        assert!(
            l.texts.iter().any(|t| t.text.starts_with("Cannot detach")),
            "and it says so rather than asking"
        );
    }

    #[test]
    fn the_confirm_never_hands_the_wheel_to_the_strip_behind_it() {
        // The exhaustive `wheel_target` exists because regions drawn inside
        // the grid area used to fall through to "must be the strip" (#256).
        // A modal is the clearest case: nothing behind it may move.
        for region in [
            HitRegion::ConfirmPanel,
            HitRegion::ConfirmClose,
            HitRegion::ConfirmDetach,
            HitRegion::ConfirmCancel,
        ] {
            assert_eq!(
                super::super::hit::wheel_target(Some(region), None),
                super::super::hit::WheelTarget::Swallow,
                "{region:?} must swallow the wheel"
            );
        }
    }

    #[test]
    fn an_unseen_signal_marks_its_tab_in_both_positions() {
        // #383. The dot has to survive both layouts, and it reaches them
        // differently: the chip gets a *badge* on its glyph tile because that
        // tile's ink already carries `LinkKind` degradation and one mark
        // cannot honestly say two things, while the sidebar row has one dot
        // and no link ink to collide with, so it simply recolours.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let mut lit = tab(1, TabOrigin::Local, TabPresence::Online);
            lit.attention = Some(zest_proto::AttentionCause::Bell);
            let m = metrics(1200.0, 800.0, 1.0);
            let info = colors().info;

            let quiet_l =
                layout(&model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], position),
                       &colors(), &m, &mut measure);
            let lit_l = layout(&model(vec![lit], position), &colors(), &m, &mut measure);

            let info_rects = |l: &ChromeLayout| {
                l.rects.iter().filter(|r| r.fill == info).count()
            };
            assert!(
                info_rects(&lit_l) > info_rects(&quiet_l),
                "{position:?}: a tab that asked to be noticed draws something in ui.info \
                 that a quiet one does not"
            );
        }
    }

    #[test]
    fn a_busy_tab_says_so_in_both_positions() {
        // #385's first half, and it is free: `running` has been computed for
        // every tab since the sidebar's dot was written and read at exactly
        // one site, so the horizontal strip showed nothing at all while a
        // command ran.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let m = metrics(1200.0, 800.0, 1.0);
            let quiet = layout(
                &model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], position),
                &colors(),
                &m,
                &mut measure,
            );
            let mut busy_tab = tab(1, TabOrigin::Local, TabPresence::Online);
            busy_tab.running = true;
            let busy = layout(&model(vec![busy_tab], position), &colors(), &m, &mut measure);
            // Counted by *ink*, not by rect count: the chip adds a ring while
            // the sidebar recolours the dot it already had, so the two
            // positions cannot be asserted the same way by shape. `warn` is
            // the running colour in both (design §2), and the default
            // `AnimPhase` has the pulse at full.
            let warn = colors().warn;
            let warned = |l: &ChromeLayout| {
                l.rects.iter().filter(|r| r.fill == warn || r.border == warn).count()
            };
            assert!(
                warned(&busy) > warned(&quiet),
                "{position:?}: a tab running something has to look different from one that is not                  ({} vs {} warn marks)",
                warned(&busy),
                warned(&quiet)
            );
        }
    }

    #[test]
    fn progress_and_a_running_block_are_two_facts_not_one() {
        // Neither implies the other. `running` is the shell's word (OSC 133,
        // so silent under bash and under a TUI); `progress` is the program's
        // own (OSC 9;4). A tab with either is busy, and a tab whose job says
        // it *failed* must not go on looking like one that is merely running.
        let m = metrics(1200.0, 800.0, 1.0);
        let with = |running, progress| {
            let mut t = tab(1, TabOrigin::Local, TabPresence::Online);
            t.running = running;
            t.progress = progress;
            layout(&model(vec![t], TabsPosition::Left), &colors(), &m, &mut measure)
        };
        let inked = |l: &ChromeLayout, c| {
            l.rects.iter().filter(|r| r.fill == c || r.border == c).count()
        };

        let idle = with(false, zest_core::Progress::None);
        let only_progress = with(false, zest_core::Progress::Indeterminate);
        assert!(
            inked(&only_progress, colors().warn) > inked(&idle, colors().warn),
            "a program reporting progress is busy even with no block saying so"
        );
        let failed = with(
            true,
            zest_core::Progress::At { percent: 80, state: zest_core::ProgressState::Error },
        );
        assert!(
            inked(&failed, colors().danger) > inked(&idle, colors().danger),
            "and a job that says it failed says so, ahead of the block index              which may not know yet"
        );
        assert_eq!(
            inked(&failed, colors().warn),
            inked(&idle, colors().warn),
            "failed is not also 'running': the row has one dot, so the newer              fact has to win outright"
        );
    }

    #[test]
    fn a_finished_bar_is_a_closed_ring_and_a_still_one() {
        // 100% is exactly the fraction that "a fraction of 1.0 means spin"
        // collides with, so the one state meaning *finished* rendered as the
        // one meaning *still going*. Two properties, and the second is what
        // the first was hiding: a full ring has no gap in it, and it does not
        // change when the clock does.
        let m = metrics(1200.0, 800.0, 1.0);
        let at = |percent, spin| {
            let mut t = tab(1, TabOrigin::Local, TabPresence::Online);
            t.progress =
                zest_core::Progress::At { percent, state: zest_core::ProgressState::Normal };
            let mut mo = model(vec![t], TabsPosition::Top);
            mo.anim.spin = spin;
            layout(&mo, &colors(), &m, &mut measure)
        };
        let full = at(100, 0.0);
        let half = at(50, 0.0);
        assert!(
            full.rects.len() < half.rects.len(),
            "a closed ring draws fewer gaps than a half-full one ({} vs {})",
            full.rects.len(),
            half.rects.len()
        );

        // And the clock does not move it. An indeterminate one *must* move,
        // which is what makes this a distinction rather than a preference.
        let a = at(100, 0.0);
        let b = at(100, 0.6);
        assert_eq!(
            a.rects.len(),
            b.rects.len(),
            "a finished bar is the same picture at every phase of the clock"
        );

        let spin_at = |spin| {
            let mut t = tab(1, TabOrigin::Local, TabPresence::Online);
            t.progress = zest_core::Progress::Indeterminate;
            let mut mo = model(vec![t], TabsPosition::Top);
            mo.anim.spin = spin;
            let l = layout(&mo, &colors(), &m, &mut measure);
            // Both axes: the bite's x is identical at phases 0.0 and 0.5
            // (cos is zero at both quarter turns), so comparing one would
            // report a ring that turns perfectly well as frozen.
            l.rects.iter().map(|r| (r.rect[0].to_bits(), r.rect[1].to_bits())).collect::<Vec<_>>()
        };
        assert_ne!(spin_at(0.0), spin_at(0.5), "an indeterminate ring turns");
    }

    #[test]
    fn a_signal_does_not_move_anything_it_marks() {
        // The badge hangs off the glyph tile's corner and the sidebar's dot
        // is recoloured in place, so neither costs a pixel of the title's
        // budget. A mark that reflowed the row would read as the tab changing
        // rather than as one asking for you.
        for position in [TabsPosition::Top, TabsPosition::Left] {
            let m = metrics(1200.0, 800.0, 1.0);
            let width_of = |attention| {
                let mut t = tab(1, TabOrigin::Local, TabPresence::Online);
                t.attention = attention;
                let l = layout(&model(vec![t], position), &colors(), &m, &mut measure);
                l.texts.iter().find(|t| t.text == "tab 1").map(|t| t.max_width)
                    .expect("the tab draws its title")
            };
            assert_eq!(
                width_of(None),
                width_of(Some(zest_proto::AttentionCause::Bell)),
                "{position:?}: the title's budget is the same either way"
            );
        }
    }

    #[test]
    fn a_pairing_notice_is_readable_over_the_grid_and_takes_no_clicks() {
        // #190: while a remote attach waits for approval, the person must be
        // able to read the six-digit matching code somewhere better than a
        // log line. The bar must sit in the grid area (a 34px chip cannot
        // carry a sentence), stay below the modal overlays, and answer no
        // clicks — a hit region here would steal the terminal's top rows
        // from selection.
        let text = "waiting for approval on forge — code 481502 · 2m left";
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.notice = Some(text.into());
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&mo, &colors(), &m, &mut measure);

        let idx = l
            .texts
            .iter()
            .position(|t| t.text == text)
            .expect("the notice text must be drawn");
        let run = &l.texts[idx];
        let [gx, gy, gw, _] = mo.grid_area;
        assert!(
            run.pos[0] > gx && run.pos[0] < gx + gw && run.pos[1] > gy,
            "the notice belongs in the grid area, where the eye already is: {:?}",
            run.pos
        );
        assert!(
            idx < l.overlay_texts_at,
            "the notice is base chrome — a modal (picker, palette) must be able to cover it"
        );
        let c = colors();
        assert!(
            l.rects.iter().any(|r| r.border == c.warn && r.rect[1] > gy),
            "the bar wears warn ink — the 'a person is needed' colour the chips use"
        );
        assert!(
            l.hit.hit(600.0, run.pos[1]).is_none(),
            "the bar must not answer clicks, or it steals the grid's own rows"
        );

        // And no notice draws nothing — the common case pays zero.
        mo.notice = None;
        let quiet = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            quiet.texts.iter().all(|t| t.text != text),
            "a cleared prompt must leave the chrome"
        );
    }

    #[test]
    fn a_profile_accent_reaches_the_rule_and_the_tile() {
        // §12's one per-tab chrome concession: the 2px inset rule and the
        // glyph tile draw the tab's own accent. Both, from one choice — a
        // chip whose rule and tile disagree names two identities at once.
        let c = colors();
        let mut tabs = vec![tab(1, TabOrigin::Local, TabPresence::Online)];
        // Index 3 of the theme's accent row is `danger` — the k8s-prod red.
        tabs[0].tab_accent = AccentChoice::Profile(3);
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(tabs, TabsPosition::Top), &c, &m, &mut measure);
        assert!(
            l.rects
                .iter()
                .any(|r| r.border == c.danger && (r.border_width - ACCENT_RULE).abs() < 1e-6),
            "the active chip's 2px rule takes the profile's colour"
        );
        assert!(
            l.rects.iter().any(|r| r.fill == c.danger && r.rect[1] < 46.0),
            "and so does the glyph tile's ink"
        );
    }

    #[test]
    fn the_active_chips_accent_tapers_rather_than_being_cut() {
        // `box-shadow: inset 0 2px 0` on a `9px 9px 0 0` box thins to nothing
        // where each corner stops being horizontal. This was a full ring
        // *clipped* to the top `TAB_RADIUS + ACCENT_RULE` band, which is a
        // different picture: a clip cuts a full-weight stroke off square, so
        // the chip wore two 2px stubs down its sides ending in a hard step.
        // The taper has to be geometry — no clip can produce it.
        let c = colors();
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top), &c, &m, &mut measure);
        let accent = l
            .rects
            .iter()
            .find(|r| (r.border_width - ACCENT_RULE * m.scale).abs() < 1e-6)
            .expect("the active chip draws its accent rule");
        assert_eq!(
            accent.border_omit,
            border_sides::RIGHT | border_sides::BOTTOM | border_sides::LEFT,
            "top only — the omitted sides are what the stroke tapers into"
        );
        assert!(
            accent.clip[3] > (TAB_RADIUS + ACCENT_RULE) * m.scale,
            "the clip is the strip viewport, not a band cutting the rule short"
        );
    }

    #[test]
    fn the_active_chip_has_no_bottom_border() {
        // "border-bottom: none" — the chip's fill has to meet the pane with
        // nothing drawn between. The ring used to be a hairline taller than the
        // chip so its bottom edge fell outside the strip clip; that worked, but
        // it made the rect a lie about the chip's geometry and every reader had
        // to know why.
        let c = colors();
        let m = metrics(1200.0, 800.0, 1.0);
        let l = layout(&model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top), &c, &m, &mut measure);
        let ring = l
            .rects
            .iter()
            .find(|r| r.border == c.line && r.radii == [TAB_RADIUS * m.scale, TAB_RADIUS * m.scale, 0.0, 0.0])
            .expect("the active chip is outlined");
        assert!((ring.border_width - HAIRLINE * m.scale).abs() < 1e-6, "a hairline, as `border: 1px` is");
        assert_eq!(ring.border_omit, border_sides::BOTTOM);
        assert!(
            (ring.rect[3] - TAB_H * m.scale).abs() < 1e-6,
            "the rect is the chip, not the chip plus a hairline of slack"
        );
    }

    #[test]
    fn the_vertical_header_spans_the_window() {
        // Design §2: one 46px header edge to edge — a header stopping at the
        // sidebar left a dead gap above it on Windows, where the caption
        // buttons live top-right.
        let m = metrics(1200.0, 800.0, 1.0);
        let mo = borderless(
            vec![tab(1, TabOrigin::Local, TabPresence::Online)],
            TabsPosition::Left,
        );
        let l = layout(&mo, &colors(), &m, &mut measure);

        // Every x across the window answers as chrome inside the header band
        // (below the resize band, which owns the top pixels).
        for x in [5.0, 300.0, 600.0, 900.0, 1190.0] {
            assert!(
                l.hit.hit(x, 20.0).is_some(),
                "the header must own ({x}, 20): it runs edge to edge"
            );
        }
        // ⌘K lives in the header, left of the caption cluster.
        let pill_x = (0..1200)
            .map(|x| x as f32)
            .find(|&x| l.hit.hit(x, 22.0) == Some(HitRegion::PalettePill))
            .expect("⌘K is in the header");
        let captions = caption_rects(&l, &m, 22.0);
        assert_eq!(captions.len(), 3, "the drawn caption cluster sits in the header");
        assert!(
            pill_x < captions[0].0,
            "⌘K yields to the caption reserve at the right end"
        );

        // The sidebar's top row: search pill, then the + to its right
        // (invariant 5) — both inside the sidebar.
        let sw = m.sidebar_width;
        let search_x = (0..1200)
            .map(|x| x as f32)
            .find_map(|x| {
                (46..110).map(|y| y as f32).find_map(|y| {
                    (l.hit.hit(x, y) == Some(HitRegion::SidebarSearch)).then_some(x)
                })
            })
            .expect("the search pill sits at the sidebar's top");
        let plus_x = (0..1200)
            .map(|x| x as f32)
            .find_map(|x| {
                (46..110).map(|y| y as f32).find_map(|y| {
                    (l.hit.hit(x, y) == Some(HitRegion::NewTab)).then_some(x)
                })
            })
            .expect("the + sits beside the search pill");
        assert!(search_x < plus_x, "the + is right of the search pill");
        assert!(plus_x < sw, "the + stays inside the sidebar");
    }

    /// A launcher with the full row vocabulary: two profiles (the first
    /// tagged default), the divider, and both action rows.
    fn launcher(anchor: super::super::model::LauncherAnchor) -> super::super::model::LauncherModel {
        use super::super::model::{LauncherModel, LauncherRow};
        let profile = |name: &str, default: bool, digit: u8| LauncherRow::Profile {
            name: name.into(),
            command: "pwsh -NoLogo".into(),
            host_label: None,
            default,
            digit: Some(digit),
            active: false,
            accent: AccentChoice::Profile(0),
        };
        LauncherModel {
            rows: vec![
                profile("default", true, 1),
                profile("ubuntu", false, 2),
                LauncherRow::Divider,
                LauncherRow::RunOnHost,
                LauncherRow::ManageProfiles { chord: "⌘⇧,".into() },
            ],
            selected: 0,
            anchor,
        }
    }

    #[test]
    fn the_launcher_stays_inside_the_window_from_both_anchors() {
        // §1 anchors right under the strip's `+`; §2 anchors the sidebar's
        // rightwards. Both must keep the 318px panel inside the window —
        // the sidebar one right-anchored would run off the LEFT edge, which
        // is the defect class invariant 5 names.
        use super::super::model::LauncherAnchor;
        for (position, anchor) in
            [(TabsPosition::Top, LauncherAnchor::Strip), (TabsPosition::Left, LauncherAnchor::Sidebar)]
        {
            let tabs = vec![
                tab(1, TabOrigin::Local, TabPresence::Online),
                tab(2, TabOrigin::Local, TabPresence::Online),
            ];
            let m = metrics(1200.0, 800.0, 1.0);
            let mut mo = model(tabs, position);
            mo.launcher = Some(launcher(anchor));
            let l = layout(&mo, &colors(), &m, &mut measure);

            // Panel extent, read off the hit map like a pointer would.
            let mut min_x = f32::MAX;
            let mut max_x: f32 = 0.0;
            let mut max_y: f32 = 0.0;
            let mut rows = std::collections::HashSet::new();
            for x in (0..1200).step_by(2) {
                for y in (0..800).step_by(2) {
                    match l.hit.hit(x as f32, y as f32) {
                        Some(HitRegion::LauncherPanel) | Some(HitRegion::LauncherRow(_)) => {
                            min_x = min_x.min(x as f32);
                            max_x = max_x.max(x as f32);
                            max_y = max_y.max(y as f32);
                            if let Some(HitRegion::LauncherRow(i)) = l.hit.hit(x as f32, y as f32) {
                                rows.insert(i);
                            }
                        }
                        _ => {}
                    }
                }
            }
            assert!(min_x >= 0.0 && max_x < 1200.0, "{anchor:?}: panel escapes horizontally");
            assert!(max_y < 800.0, "{anchor:?}: panel escapes the bottom");
            assert!(
                (max_x - min_x) > 300.0,
                "{anchor:?}: the panel is its designed 318px, got {}",
                max_x - min_x
            );
            // Every actionable row answers; the divider (index 2) never does.
            assert_eq!(
                rows,
                [0usize, 1, 3, 4].into(),
                "{anchor:?}: profile and action rows are hittable, the divider is not"
            );

            // The scrim is everywhere the panel is not: click-away dismisses
            // without falling through to the grid or a chip beneath.
            assert_eq!(
                l.hit.hit(600.0, 780.0),
                Some(HitRegion::LauncherScrim),
                "{anchor:?}: a far corner is the scrim's"
            );
            let panel_mid = ((min_x + max_x) / 2.0, 60.0);
            let _ = panel_mid;
        }
    }

    #[test]
    fn the_plus_wears_selsoft_and_accent_while_the_launcher_is_open() {
        // Design §1: `ui.selSoft` fill with `ui.accent` ink while its menu
        // is open — the open state must read on the button itself.
        use super::super::model::LauncherAnchor;
        let c = colors();
        let m = metrics(1200.0, 800.0, 1.0);
        let tabs = || vec![tab(1, TabOrigin::Local, TabPresence::Online)];

        let closed = layout(&model(tabs(), TabsPosition::Top), &c, &m, &mut measure);
        let plus = |l: &ChromeLayout| {
            l.texts.iter().find(|t| t.text == "+").expect("the + is drawn").color
        };
        assert_eq!(plus(&closed), c.text_inactive, "closed: dim ink");

        let mut mo = model(tabs(), TabsPosition::Top);
        mo.launcher = Some(launcher(LauncherAnchor::Strip));
        let open = layout(&mo, &c, &m, &mut measure);
        assert_eq!(plus(&open), c.accent, "open: accent ink");
        let nt = open.new_tab_rect;
        assert!(
            open.rects.iter().any(|r| r.fill == c.tab_hover_bg
                && (r.rect[0] - nt[0]).abs() < 0.5
                && (r.rect[1] - nt[1]).abs() < 0.5),
            "open: the button carries the selSoft fill"
        );
    }

    #[test]
    fn a_launcher_group_header_names_its_machine_and_answers_no_clicks() {
        // #268: the menu spans machines, so it says which. Drawn in the
        // sidebar's host-header treatment — uppercase, tracked, with a mono
        // sub — because it is the same header saying the same thing.
        //
        // And it is context, not a control: no hit region, so a click on it
        // falls through to the panel, which swallows it. A header that
        // selected a row would make the machine name a launch button.
        use super::super::model::{LauncherAnchor, LauncherModel, LauncherRow};
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);
        mo.launcher = Some(LauncherModel {
            rows: vec![
                LauncherRow::Group {
                    label: "forge".into(),
                    sub: "windows \u{b7} LAN \u{b7} 0.4 ms".into(),
                    online: true,
                },
                LauncherRow::Profile {
                    name: "ubuntu".into(),
                    command: "wsl.exe".into(),
                    host_label: None,
                    default: true,
                    digit: Some(1),
                    active: false,
                    accent: AccentChoice::Profile(0),
                },
            ],
            selected: 1,
            anchor: LauncherAnchor::Strip,
        });
        let out = layout(&mo, &colors(), &m, &mut measure);

        assert!(
            out.texts.iter().any(|t| t.text == "FORGE"),
            "the header names the machine, uppercased like every other group label"
        );
        assert!(
            out.texts.iter().any(|t| t.text.contains("windows")),
            "and its sub-label says what that machine is and how we reach it"
        );
        // The header's own band must answer no clicks. Find it by the row it
        // is drawn above: the profile row takes a hit region, the header does
        // not, so probing inside the header's band must not name row 0.
        // The panel is right-anchored under the `+`, so probe from its own
        // rect rather than guessing a column.
        let px = out.new_tab_rect[0] + out.new_tab_rect[2] - LAUNCHER_W / 2.0;
        let profile_hit = (0..800)
            .find(|y| out.hit.hit(px, *y as f32) == Some(HitRegion::LauncherRow(1)))
            .expect("the profile row is clickable");
        assert!(
            (0..profile_hit).all(|y| out.hit.hit(px, y as f32) != Some(HitRegion::LauncherRow(0))),
            "the header takes no hit region anywhere above it — it is context, not a control"
        );
    }

    #[test]
    fn a_launcher_row_draws_its_host_chip_exactly_when_pinned() {
        // The chip now tells the truth (issue #175): the launch honours the
        // profile's host key, so a pinned row names its machine — as text,
        // per the design's origin rule — and an unpinned row draws nothing,
        // the dead-affordance rule.
        use super::super::model::{LauncherAnchor, LauncherModel, LauncherRow};
        let row = |host_label: Option<&str>| LauncherRow::Profile {
            name: "ubuntu".into(),
            command: "wsl.exe".into(),
            host_label: host_label.map(str::to_string),
            default: false,
            digit: Some(1),
            active: false,
            accent: AccentChoice::Profile(0),
        };
        let m = metrics(1200.0, 800.0, 1.0);
        let mut mo = model(vec![tab(1, TabOrigin::Local, TabPresence::Online)], TabsPosition::Top);

        mo.launcher = Some(LauncherModel {
            rows: vec![row(Some("forge"))],
            selected: 0,
            anchor: LauncherAnchor::Strip,
        });
        let pinned = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            pinned.texts.iter().any(|t| t.text == "forge"),
            "a pinned profile's row carries its host as text"
        );

        mo.launcher = Some(LauncherModel {
            rows: vec![row(None)],
            selected: 0,
            anchor: LauncherAnchor::Strip,
        });
        let unpinned = layout(&mo, &colors(), &m, &mut measure);
        assert!(
            !unpinned.texts.iter().any(|t| t.text == "forge"),
            "no pin, no chip — a chip naming a machine the launch will not use"
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
