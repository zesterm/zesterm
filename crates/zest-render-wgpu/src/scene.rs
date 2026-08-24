//! Building instance lists from a terminal grid.
//!
//! The renderer takes a slice of viewports from day one, even though M1 always
//! passes exactly one. Panes then cost a loop and a clip rect rather than a
//! restructuring of the render loop. → ADR-003 / ROADMAP sequencing.

use zest_core::grid::Row;
use zest_core::{Cell, CellFlags, Color, CursorShape, Grid, PaletteSnapshot};
use zest_font::{CellMetrics, Fonts, GlyphKey, Style};

use crate::atlas::{Atlas, Cached};
use crate::image::{source_rect, BackgroundImage};
use crate::instance::{
    glyph_flags, DecorInstance, DecorKind, GlyphInstance, ImageInstance, LinearRgba, RectInstance,
};

/// The block rail's width, physical px. Matches the header band's own rail
/// (`chrome::blocks::RAIL`) so the two read as one rule down the block —
/// they are drawn in different layers and cannot share a constant.
///
/// Public because it is also the *threshold*: below this much
/// [`Viewport::gutter`] there is nowhere honest to put the rail and it is
/// silently not drawn, so whoever computes a gutter has to be able to assert
/// against the number rather than a copy of it. A split pane's gutter was 0.0
/// by construction for exactly as long as no test could say so (#460).
pub const RAIL_PX: f32 = 2.0;

/// Breathing room between the rail and the first column, physical px. Without
/// it the rail reads as a left border on the text rather than as the block's
/// own edge.
const RAIL_GAP: f32 = 4.0;

/// One command block's decoration: the state rail down its whole height, and
/// a wash over it when it is the selected one.
///
/// **Absolute line ids, not viewport rows**, exactly like [`Viewport::selection`]
/// — and for the same reason. A block's decoration has to survive scrolling
/// (the lines move under the viewport), folding (the hidden rows are simply
/// absent from `row_map`), a header that has scrolled off the top, and the
/// clip at both edges. Named by row, every one of those is a case to handle;
/// named by line, they are all the row loop asking `resolved_row` what it is
/// drawing, which it does anyway.
///
/// This is also why it lives here rather than with the block *header* chrome,
/// which is drawn a layer up: base-chrome rects paint over the grid's glyphs,
/// so a rail drawn there would shave the left edge off column 0 on every
/// output row, and a wash would erase the output outright.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockBand {
    /// Half-open absolute line range, `[from, to)`.
    pub from: u64,
    pub to: u64,
    pub rail: LinearRgba,
    /// The selected block's wash over its rows; `None` for every other block.
    ///
    /// Kept translucent by its caller: this is painted *under* the glyphs, but
    /// an opaque fill would still flatten every cell background it covers.
    pub wash: Option<LinearRgba>,
}

/// One terminal view: a grid, where to draw it, and how it is coloured.
pub struct Viewport<'a> {
    /// Where this viewport sits, in physical pixels.
    pub rect: [f32; 4],
    pub grid: &'a Grid,
    pub palette: &'a PaletteSnapshot,
    /// Sub-row scroll offset in pixels, for smooth scrolling.
    pub scroll_px: f32,
    pub focused: bool,
    /// Window background opacity.
    ///
    /// **Applies only to cells whose background is [`Color::Default`].** A cell
    /// with an explicit background — `ls` colours, a TUI panel, an
    /// `@sigx/terminal-ui` box — must stay opaque. Applying opacity to every
    /// cell double-darkens *and* makes every TUI look broken. → ADR-003.
    pub opacity: f32,
    /// A picture drawn behind this pane's cells, in place of its plain
    /// background.
    ///
    /// **Per viewport, not per window**, and the reason is ADR-012: a profile's
    /// scheme applies to its grid only, so two panes running two profiles can
    /// carry two different pictures and the chrome stays the window's
    /// throughout. It reaches the cells through exactly the same rule as
    /// [`Self::opacity`] — a default background emits no rect, so the picture
    /// shows through it, and an explicit one paints over the picture just as it
    /// paints over the window colour.
    pub background: Option<BackgroundImage>,
    /// Command blocks to decorate: a rail down each, a wash under the selected
    /// one. Empty on the everyday path and for a session with no shell
    /// integration loaded.
    pub blocks: &'a [BlockBand],
    /// Free pixels immediately left of `rect`, for the block rail to live in.
    ///
    /// The rail cannot go *inside* the grid: chrome paints over the glyphs, so
    /// drawing it a layer up shaves the left edge off column 0 on every row,
    /// and drawing it here — under the glyphs, which is correct — means any
    /// line starting at column 0 hides it. A rail that appears and disappears
    /// with the indentation of the output reads as a rendering fault, so it
    /// gets its own space or it does not draw at all.
    ///
    /// Comes from the window padding and the letterbox slack, which is space
    /// no cell can ever occupy. `0.0` (padding turned off, a grid that fits
    /// exactly) means no rail — the header band still carries its own.
    pub gutter: f32,
    /// The active selection, if any.
    pub selection: Option<zest_core::Selection>,
    /// Colour of the selection highlight.
    pub selection_bg: zest_core::Rgb,
    /// Text an input method is still composing, drawn over the cursor.
    ///
    /// **Not in the grid, deliberately.** A composition is provisional and
    /// belongs to the keyboard in front of this person, while the grid is shared
    /// with the daemon and with every other device attached to the session.
    /// Drawing it here rather than writing it into cells is what keeps
    /// half-typed characters out of someone else's scrollback.
    pub preedit: Option<Preedit<'a>>,
    /// Keystrokes whose echo the host has not confirmed yet, drawn as guesses.
    ///
    /// The same seam as the preedit, for the same reason: a guess belongs to
    /// the keyboard in front of this person and must never reach the grid
    /// that every other attached device, the block index and an agent read.
    /// `None` and an empty list draw nothing; the cursor moves to the caret
    /// only while there is something to draw. → ADR-016.
    pub predicted: Option<Predicted<'a>>,
    /// Blink phase: `false` hides the focused block cursor (the off half of
    /// the cycle). The hollow unfocused cursor never blinks — it marks where
    /// focus would land, and a vanishing landmark is worse than none.
    pub cursor_on: bool,
    /// How far the drawn caret lags its real cell, in physical pixels.
    ///
    /// `cursor.trail`. Visual only: the grid's cursor is wherever the program
    /// put it, so input, IME placement and hit testing are unaffected by an
    /// animation still in flight — which is what keeps a trail a decoration
    /// rather than a source of truth.
    pub cursor_offset: [f32; 2],
    /// OpenType features to shape the grid with, and whether ligatures may
    /// form. Empty and `false` is the shipped default, and is what keeps the
    /// per-character fast path -- see [`Scene::emit_row_glyphs`].
    pub features: &'a [zest_font::Feature],
    pub ligatures: bool,
    /// The shape a focused cursor draws.
    ///
    /// Already resolved by the caller: `cursor.shape` is the default and
    /// DECSCUSR overrides it, and deciding that here would put the policy in
    /// the renderer where the config cannot reach it.
    pub cursor_shape: CursorShape,
    /// Folded-view row map: for each visual row, the *absolute* storage index
    /// of the grid row to draw there ([`Grid::line`]'s argument), or
    /// `usize::MAX` for a blank filler when history ran out. `None` draws the
    /// plain viewport — the everyday fast path, untouched.
    ///
    /// This is the fold seam the roadmap planned: the row loop compacts over
    /// hidden ranges, and selection and the cursor read the same list, so a
    /// folded build's rows cannot drift from where clicks land (WS-E).
    pub row_map: Option<&'a [usize]>,
}

/// The row a visual row shows: mapped through the fold view when one is
/// active, the viewport row otherwise. `None` is a blank filler row.
fn resolved_row<'g>(grid: &'g Grid, vp: &Viewport<'_>, row: usize) -> Option<&'g Row> {
    match vp.row_map {
        Some(map) => match map.get(row) {
            Some(&i) if i != usize::MAX => grid.line(i),
            _ => None,
        },
        None => Some(grid.row(row)),
    }
}

/// Where the cursor's grid position lands visually, if it is on screen.
fn cursor_visual_row(grid: &Grid, vp: &Viewport<'_>) -> Option<usize> {
    match vp.row_map {
        Some(map) => {
            let abs = grid.abs_index(grid.cursor.row);
            map.iter().position(|&i| i == abs)
        }
        None => Some(grid.cursor.row),
    }
}

/// One guessed cell. `ch` is one cell wide by construction — the predictor
/// refuses anything whose width only the host knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredictedCell {
    pub row: u16,
    pub col: u16,
    pub ch: char,
}

/// The guesses to draw, and where the caret belongs while they stand.
#[derive(Debug, Clone, Copy)]
pub struct Predicted<'a> {
    pub cells: &'a [PredictedCell],
    /// After the last guess, so the line reads as the user typed it.
    pub caret: (u16, u16),
}

/// Composing text and the input method's own caret within it.
#[derive(Debug, Clone, Copy)]
pub struct Preedit<'a> {
    pub text: &'a str,
    /// Byte range the input method has highlighted, as winit reports it.
    pub cursor: Option<(usize, usize)>,
}

/// Pre-built chrome instances.
///
/// Empty in M1; the tab bar and titlebar fill it later. Present now so the
/// render signature does not change when they arrive.
#[derive(Debug, Default)]
pub struct Chrome {
    pub rects: Vec<RectInstance>,
    pub glyphs: Vec<GlyphInstance>,
    /// Index into `rects`/`glyphs` where the *overlay* layer (picker,
    /// palette, settings) begins. Base chrome — bars, tabs, screens, block
    /// headers — must finish its text before an overlay's panel draws, or
    /// that text bleeds through the panel. Both equal to the vec length (or
    /// 0 with the vecs empty) when nothing overlays.
    pub overlay_rects_at: usize,
    pub overlay_glyphs_at: usize,
}

/// Everything to draw this frame.
#[derive(Debug, Default)]
pub struct Scene {
    pub rects: Vec<RectInstance>,
    pub glyphs: Vec<GlyphInstance>,
    pub decors: Vec<DecorInstance>,
    /// One per viewport that carries a picture, drawn before every rect.
    pub images: Vec<ImageInstance>,
    /// What every pixel no instance covers is painted with.
    ///
    /// The grid does not own the window — `window.padding`, the gap between the
    /// tab strip and the grid, and the gutter between split panes all lie
    /// outside every viewport rect. Nothing draws there, so without a backdrop
    /// those pixels stay `(0,0,0,0)`, and an opaque surface discards the alpha
    /// and composites them as black: a black frame around the terminal, whatever
    /// the theme.
    pub backdrop: LinearRgba,
    /// Sub-pixel grid translation, applied in the vertex shader.
    pub grid_origin: [f32; 2],
    /// Index in `rects` where the chrome's instances begin.
    ///
    /// The buffers are shared but the draw order is not: the renderer draws
    /// grid instances, then chrome, in split ranges. Without the split every
    /// grid glyph paints *after* — and therefore over — the chrome's panels,
    /// which is exactly the picker-behind-the-prompt bug this retired.
    pub chrome_rects_at: usize,
    /// Index in `glyphs` where the chrome's instances begin.
    pub chrome_glyphs_at: usize,
    /// Absolute index where the chrome's overlay layer begins (see
    /// [`Chrome::overlay_rects_at`]).
    pub overlay_rects_at: usize,
    pub overlay_glyphs_at: usize,
}

impl Scene {
    /// Reuse the allocations. Called every frame; allocating fresh vectors for
    /// 30k instances per frame would show up in the profile.
    pub fn clear(&mut self) {
        self.rects.clear();
        self.glyphs.clear();
        self.decors.clear();
        self.images.clear();
        self.backdrop = LinearRgba::TRANSPARENT;
        self.grid_origin = [0.0, 0.0];
        self.chrome_rects_at = 0;
        self.chrome_glyphs_at = 0;
        self.overlay_rects_at = 0;
        self.overlay_glyphs_at = 0;
    }

    /// Build a frame.
    ///
    /// Rasterizes any glyph the atlas is missing, which is why this needs
    /// `Fonts` and the GPU queue. In steady state there are no misses at all.
    ///
    /// `backdrop` is what the window is painted with everywhere the viewports do
    /// not reach — see [`Scene::backdrop`]. Linear premultiplied like every other
    /// colour here (ADR-003).
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        metrics: CellMetrics,
        backdrop: LinearRgba,
        viewports: &[Viewport<'_>],
        chrome: &Chrome,
    ) {
        self.clear();
        self.backdrop = backdrop;

        for vp in viewports {
            self.build_viewport(device, queue, atlas, fonts, metrics, vp);
        }

        self.append_chrome(chrome);
    }

    /// Chrome last, so it sits above the grid — and the boundary recorded, so
    /// the renderer can actually draw it above rather than merely after it in
    /// the same buffer.
    fn append_chrome(&mut self, chrome: &Chrome) {
        self.chrome_rects_at = self.rects.len();
        self.chrome_glyphs_at = self.glyphs.len();
        self.overlay_rects_at = self.chrome_rects_at + chrome.overlay_rects_at.min(chrome.rects.len());
        self.overlay_glyphs_at =
            self.chrome_glyphs_at + chrome.overlay_glyphs_at.min(chrome.glyphs.len());
        self.rects.extend_from_slice(&chrome.rects);
        self.glyphs.extend_from_slice(&chrome.glyphs);
    }

    /// The viewport's own background, when the backdrop is not already it.
    ///
    /// One instance rather than one per blank cell — and none at all in the
    /// everyday case, where this viewport's background *is* what the offscreen
    /// was cleared to. Skipping it is not thrift: at `opacity < 1` a second
    /// translucent rect over the backdrop composites to `1-(1-o)²`, so the grid
    /// would come out visibly less transparent than the padding around it. The
    /// instance still goes in when a session set its own background (OSC 11), or
    /// when two split panes are on different palettes and only one of them can
    /// be the backdrop.
    fn push_window_background(&mut self, vp: &Viewport<'_>) {
        let window = window_bg(vp.palette, vp.opacity);

        // A picture *is* this pane's window background, so it goes in instead
        // of the rect -- and unconditionally, where the rect is skipped when
        // the clear already painted it. The quad carries `window` itself and
        // is drawn with the blend disabled, so at `dim = 1` it writes exactly
        // what the rect would have and the double-composite the branch below
        // avoids cannot happen here either.
        if let Some(bg) = vp.background {
            self.images.push(ImageInstance::new(
                vp.rect,
                vp.rect,
                source_rect(bg.fit, bg.size, [vp.rect[2], vp.rect[3]]),
                window,
                bg.dim,
                bg.image,
            ));
            return;
        }

        if window != self.backdrop {
            self.rects.push(RectInstance::filled(vp.rect, window, vp.rect));
        }
    }

    fn build_viewport(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        metrics: CellMetrics,
        vp: &Viewport<'_>,
    ) {
        let clip = vp.rect;
        let (ox, oy) = (vp.rect[0], vp.rect[1]);
        let cw = metrics.cell_w as f32;
        let ch = metrics.cell_h as f32;
        let grid = vp.grid;

        self.grid_origin = [0.0, -vp.scroll_px];

        self.push_window_background(vp);

        for row in 0..grid.rows() {
            let y = oy + row as f32 * ch;
            self.emit_row_backgrounds(grid, row, vp, ox, y, cw, ch, clip);
        }

        // Block decoration first, then the selection over it: a dragged
        // selection is the more specific answer about the same rows, and the
        // one the user is making right now.
        self.emit_block_bands(grid, vp, ox, oy, cw, ch, clip);

        // Selection sits above cell backgrounds but below the glyphs, so text
        // stays readable through it.
        self.emit_selection(grid, vp, ox, oy, cw, ch, clip);

        for row in 0..grid.rows() {
            let y = oy + row as f32 * ch;
            self.emit_row_glyphs(device, queue, atlas, fonts, metrics, grid, row, vp, ox, y, clip);
        }

        // The preedit covers the cursor cell and the ones after it, so the block
        // cursor is skipped while composing: the input method draws its own
        // caret inside the composing text, and two caret-like blocks in the same
        // place is worse than either alone.
        if let Some(pre) = vp.preedit {
            self.emit_preedit(device, queue, atlas, fonts, metrics, grid, vp, &pre, ox, oy, clip);
        } else {
            let caret = vp
                .predicted
                .filter(|p| !p.cells.is_empty())
                .map(|p| (usize::from(p.caret.0), usize::from(p.caret.1)));
            if let Some(p) = vp.predicted {
                self.emit_predicted(device, queue, atlas, fonts, metrics, grid, vp, &p, ox, oy, clip);
            }
            self.emit_cursor(grid, vp, metrics, ox, oy, clip, caret);
        }
    }

    /// Guessed echo, drawn over the grid one cell at a time.
    ///
    /// Dim and underlined — the glyph says what was typed, the treatment says
    /// the host has not agreed yet — and on the default background only where
    /// the cell underneath already shows something, so a guess over blank
    /// space costs one glyph and no rectangle. Suppressed while scrolled back
    /// for the cursor's reason — the cells it names are not on screen — and
    /// in a folded view, where a viewport row is not a grid row and a guess
    /// on a folded line has nowhere honest to land.
    #[allow(clippy::too_many_arguments)]
    fn emit_predicted(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        metrics: CellMetrics,
        grid: &Grid,
        vp: &Viewport<'_>,
        p: &Predicted<'_>,
        ox: f32,
        oy: f32,
        clip: [f32; 4],
    ) {
        if grid.display_offset() != 0 || vp.row_map.is_some() {
            return;
        }
        let (cw, ch) = (metrics.cell_w as f32, metrics.cell_h as f32);
        // SGR dim's own alpha, so a guess reads exactly as dim text does.
        let fg = LinearRgba::from_srgb(
            vp.palette.foreground.r,
            vp.palette.foreground.g,
            vp.palette.foreground.b,
            0.55,
        );
        let bg = LinearRgba::opaque(
            vp.palette.background.r,
            vp.palette.background.g,
            vp.palette.background.b,
        );
        for cell in p.cells {
            let (row, col) = (usize::from(cell.row), usize::from(cell.col));
            if row >= grid.rows() || col >= grid.cols() {
                continue;
            }
            let x = ox + col as f32 * cw;
            let y = oy + row as f32 * ch;
            if grid.row(row).cells()[col].ch != ' ' {
                self.rects.push(RectInstance::filled([x, y, cw, ch], bg, clip));
            }
            if let Some(inst) = self.glyph_instance(
                device,
                queue,
                atlas,
                fonts,
                cell.ch,
                Style::new(false, false),
                x,
                y + metrics.baseline as f32,
                fg,
                clip,
            ) {
                self.glyphs.push(inst);
            }
            self.decors.push(DecorInstance {
                rect: [x, y + metrics.underline_y as f32, cw, metrics.underline_thickness as f32],
                color: fg,
                clip,
                kind: DecorKind::Underline as u32,
                _pad: [0; 3],
            });
        }
    }

    /// Composing text, drawn over the grid starting at the cursor.
    ///
    /// Underlined and on the default background, which is what every terminal
    /// and text field does — the underline is the signal that this text is not
    /// committed yet. It is clipped to the viewport rather than wrapped: a long
    /// composition running off the right edge is momentary, and wrapping it
    /// would need a line of grid state that does not exist.
    #[allow(clippy::too_many_arguments)]
    fn emit_preedit(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        metrics: CellMetrics,
        grid: &Grid,
        vp: &Viewport<'_>,
        pre: &Preedit<'_>,
        ox: f32,
        oy: f32,
        clip: [f32; 4],
    ) {
        // Scrolled back, the cursor position refers to somewhere off screen, so
        // there is nowhere honest to put this.
        if grid.display_offset() != 0 {
            return;
        }

        let (cw, ch) = (metrics.cell_w as f32, metrics.cell_h as f32);
        let start_col = grid.cursor.col;
        let Some(visual_row) = cursor_visual_row(grid, vp) else { return };
        let y = oy + visual_row as f32 * ch;
        let baseline = y + metrics.baseline as f32;

        let fg = LinearRgba::opaque(
            vp.palette.foreground.r,
            vp.palette.foreground.g,
            vp.palette.foreground.b,
        );
        let bg = LinearRgba::opaque(
            vp.palette.background.r,
            vp.palette.background.g,
            vp.palette.background.b,
        );

        let cells: usize = pre.text.chars().map(char_cells).sum();
        let avail = grid.cols().saturating_sub(start_col);
        let drawn = cells.min(avail);
        if drawn == 0 {
            return;
        }

        let x0 = ox + start_col as f32 * cw;
        let width = drawn as f32 * cw;

        // Opaque, so whatever the composition covers does not show through it.
        self.rects.push(RectInstance::filled([x0, y, width, ch], bg, clip));

        let mut col = start_col;
        for (offset, c) in pre.text.char_indices() {
            let w = char_cells(c);
            if col + w.max(1) > grid.cols() {
                break;
            }
            let pen_x = ox + col as f32 * cw;
            if let Some(inst) = self.glyph_instance(
                device, queue, atlas, fonts, c, Style::new(false, false), pen_x, baseline, fg, clip,
            ) {
                self.glyphs.push(inst);
            }
            // The input method's caret: a thin bar rather than a block, so the
            // composing text under it stays readable.
            if pre.cursor.is_some_and(|(s, _)| s == offset) {
                let t = 1.0f32.max(metrics.underline_thickness as f32);
                self.rects.push(RectInstance::filled([pen_x, y, t, ch], fg, clip));
            }
            col += w;
        }

        // A caret at the very end has no character to anchor to.
        if pre.cursor.is_some_and(|(s, _)| s == pre.text.len()) {
            let t = 1.0f32.max(metrics.underline_thickness as f32);
            let x = ox + (col.min(grid.cols().saturating_sub(1))) as f32 * cw;
            self.rects.push(RectInstance::filled([x, y, t, ch], fg, clip));
        }

        self.decors.push(DecorInstance {
            rect: [x0, y + metrics.underline_y as f32, width, metrics.underline_thickness as f32],
            color: fg,
            clip,
            kind: DecorKind::Underline as u32,
            _pad: [0; 3],
        });
    }

    /// Background runs, collapsed by colour.
    ///
    /// A typical row is one to five runs rather than 200 cells, which is the
    /// difference between a few hundred instances per frame and tens of
    /// thousands.
    #[allow(clippy::too_many_arguments)]
    fn emit_row_backgrounds(
        &mut self,
        grid: &Grid,
        row: usize,
        vp: &Viewport<'_>,
        ox: f32,
        y: f32,
        cw: f32,
        ch: f32,
        clip: [f32; 4],
    ) {
        let Some(r) = resolved_row(grid, vp, row) else { return };
        let window = window_bg(vp.palette, vp.opacity);
        let mut run_start = 0usize;
        let mut run_color: Option<LinearRgba> = None;

        let flush = |scene: &mut Self, start: usize, end: usize, fill: LinearRgba| {
            // Skip runs that match what is already painted behind them. Compared
            // on the resolved colour rather than the `Color` reference, so an
            // inverse cell -- which resolves to a real colour -- is not mistaken
            // for a default one.
            if end <= start || fill == window || fill.is_transparent() {
                return;
            }
            scene.rects.push(RectInstance::filled(
                [ox + start as f32 * cw, y, (end - start) as f32 * cw, ch],
                fill,
                clip,
            ));
        };

        for col in 0..grid.cols() {
            let cell = r.get(col).copied().unwrap_or_default();
            let bg = cell_bg(&cell, vp.palette, vp.opacity);
            match run_color {
                Some(c) if c == bg => {}
                Some(c) => {
                    flush(self, run_start, col, c);
                    run_start = col;
                    run_color = Some(bg);
                }
                None => {
                    run_start = col;
                    run_color = Some(bg);
                }
            }
        }
        if let Some(c) = run_color {
            flush(self, run_start, grid.cols(), c);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_row_glyphs(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        metrics: CellMetrics,
        grid: &Grid,
        row: usize,
        vp: &Viewport<'_>,
        ox: f32,
        y: f32,
        clip: [f32; 4],
    ) {
        let cw = metrics.cell_w as f32;
        let baseline = y + metrics.baseline as f32;
        let Some(r) = resolved_row(grid, vp, row) else { return };

        // Shaping is opt-in, and the branch is the point rather than an
        // optimization. `glyph_for` is a charmap lookup with no GSUB, so
        // `typography.features` and `typography.ligatures` cannot work without
        // it -- and the default config asks for neither, so the overwhelming
        // majority of sessions keep the per-character path they have always
        // had, byte for byte, and the throughput targets with it.
        let shaping = !vp.features.is_empty() || vp.ligatures;
        if shaping {
            self.emit_row_runs(device, queue, atlas, fonts, r, grid, vp, ox, baseline, cw, clip);
        }

        for col in 0..grid.cols() {
            let cell = r.get(col).copied().unwrap_or_default();

            // The right half of a wide character holds no glyph of its own.
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            if cell.flags.contains(CellFlags::HIDDEN) {
                continue;
            }

            let pen_x = ox + col as f32 * cw;
            let fg = cell_fg(&cell, vp.palette);
            let style = Style::new(
                cell.flags.contains(CellFlags::BOLD),
                cell.flags.contains(CellFlags::ITALIC),
            );

            // Not `!shaping`: with shaping on, `segment_row` deliberately
            // leaves out wide cells and cells carrying combining marks, and
            // those still need their base glyph drawn. Gating on the *same*
            // predicate the segmentation uses is what stops a cell falling
            // between the two paths and being drawn by neither -- which is
            // what happened to every CJK character the first time this
            // shipped, and what ASCII-only screenshots cannot show.
            if !is_blank(&cell) && (!shaping || breaks_run(&cell, r)) {
                if let Some(inst) =
                    self.glyph_instance(device, queue, atlas, fonts, cell.ch, style, pen_x, baseline, fg, clip)
                {
                    self.glyphs.push(inst);
                }
            }

            // Combining marks live in the row's side table, not in `ch`. Drawing
            // only `ch` renders "e" where the text said "é" -- silently wrong,
            // and only for the users whose languages need it.
            if let Some(extra) = r.extra(&cell) {
                for &mark in &extra.zerowidth {
                    if let Some(inst) = self.glyph_instance(
                        device, queue, atlas, fonts, mark, style, pen_x, baseline, fg, clip,
                    ) {
                        self.glyphs.push(inst);
                    }
                }
            }

            self.emit_decorations(&cell, pen_x, y, cw, metrics, fg, clip);
        }
    }

    /// Emit a row's base glyphs as *shaped runs*, honouring `features` and
    /// `ligatures`.
    ///
    /// The trick is `shape_run`'s own: honour the shaper for glyph
    /// **selection** and ignore it for **positioning**. Each cluster is placed
    /// at the starting cell of the text it came from, so a ligature draws once,
    /// at the first of the cells it spans, and every other cell keeps its
    /// column. The grid stays a grid.
    ///
    /// A run breaks wherever the grid's model stops being a plain sequence of
    /// characters, because the shaper knows nothing about any of it:
    ///
    /// * a **style change**, since bold and italic are different faces;
    /// * a **blank**, which ends a word for shaping just as it does for reading;
    /// * a **wide cell** and its spacer, whose second column is not a character;
    /// * a cell carrying **combining marks**, which live in the row's side
    ///   table and are drawn separately;
    /// * a **hidden** cell.
    ///
    /// Selection is deliberately *not* a break: it colours backgrounds, and a
    /// ligature spanning the edge of a highlight still reads correctly. The
    /// cursor is not a break either — it is drawn over the text rather than in
    /// place of it, so a caret inside a ligature is a caret on a wide glyph,
    /// which is the same thing a caret on a CJK character already is.
    #[allow(clippy::too_many_arguments)]
    fn emit_row_runs(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        r: &Row,
        grid: &Grid,
        vp: &Viewport<'_>,
        ox: f32,
        baseline: f32,
        cw: f32,
        clip: [f32; 4],
    ) {
        for run in segment_row(r, grid.cols(), vp.palette) {
            self.flush_run(
                device, queue, atlas, fonts, &run.text, &run.starts, run.style, run.fg, ox,
                baseline, cw, clip, vp,
            );
        }
    }

    /// Shape one accumulated run and place its glyphs at their starting cells.
    #[allow(clippy::too_many_arguments)]
    fn flush_run(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        text: &str,
        starts: &[(usize, usize)],
        style: Style,
        fg: LinearRgba,
        ox: f32,
        baseline: f32,
        cw: f32,
        clip: [f32; 4],
        vp: &Viewport<'_>,
    ) {
        if text.is_empty() {
            return;
        }
        // `can_use_fast_path` asks only about ligatures, because that is the
        // question it was written for. Features have to be asked about too: an
        // ASCII run with `ss01` set would otherwise take the charmap path and
        // the feature would silently do nothing -- which is the exact bug this
        // whole sweep is closing, reintroduced one layer down.
        let shaped = if Fonts::can_use_fast_path(text, vp.ligatures) && vp.features.is_empty() {
            fonts.map_ascii(text, style)
        } else {
            fonts.shape_run(text, style, vp.features)
        };

        // `starts` ascends by byte offset and a shaper reports clusters in
        // order, so one advancing index answers every lookup. Searching the
        // list per glyph is quadratic in the run length, and a run is a whole
        // row of text.
        let mut at = 0usize;
        for g in shaped {
            // The cluster is a byte offset into `text`; the column it started
            // at is what the grid cares about.
            while at + 1 < starts.len() && starts[at + 1].0 <= g.cluster as usize {
                at += 1;
            }
            let Some(&(_, col)) = starts.get(at) else { continue };
            let pen_x = ox + col as f32 * cw;
            if let Some(inst) =
                self.glyph_at(device, queue, atlas, fonts, g.font, g.glyph, pen_x, baseline, fg, clip)
            {
                self.glyphs.push(inst);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn glyph_instance(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        ch: char,
        style: Style,
        pen_x: f32,
        baseline: f32,
        color: LinearRgba,
        clip: [f32; 4],
    ) -> Option<GlyphInstance> {
        let (font, glyph) = fonts.glyph_for(ch, style)?;
        self.glyph_at(device, queue, atlas, fonts, font, glyph, pen_x, baseline, color, clip)
    }

    /// [`Self::glyph_instance`] for a glyph the caller has already resolved —
    /// which is what shaping produces, and the reason the two are split.
    #[allow(clippy::too_many_arguments)]
    fn glyph_at(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        font: zest_font::FontId,
        glyph: u16,
        pen_x: f32,
        baseline: f32,
        color: LinearRgba,
        clip: [f32; 4],
    ) -> Option<GlyphInstance> {
        let key: GlyphKey = fonts.key(font, glyph);

        let cached = match atlas.get(&key) {
            Some(c) => c,
            None => {
                let image = fonts.rasterize(key)?;
                atlas.insert(device, queue, key, &image)?
            }
        };

        let Cached::Entry(e) = cached else { return None };

        Some(GlyphInstance {
            // Bearing is baked in on the CPU so the shader stays a plain quad.
            pos: [pen_x + f32::from(e.left), baseline - f32::from(e.top)],
            uv: [f32::from(e.uv[0]), f32::from(e.uv[1])],
            size: [f32::from(e.size[0]), f32::from(e.size[1])],
            color,
            clip,
            layer: u32::from(e.layer),
            flags: if e.is_color { glyph_flags::COLOR } else { 0 },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_decorations(
        &mut self,
        cell: &Cell,
        x: f32,
        y: f32,
        width: f32,
        metrics: CellMetrics,
        color: LinearRgba,
        clip: [f32; 4],
    ) {
        let thickness = metrics.underline_thickness as f32;
        let mut push = |kind: DecorKind, top: f32| {
            self.decors.push(DecorInstance {
                rect: [x, top, width, thickness],
                color,
                clip,
                kind: kind as u32,
                _pad: [0; 3],
            });
        };

        let underline_y = y + metrics.underline_y as f32;
        if cell.flags.contains(CellFlags::UNDERCURL) {
            push(DecorKind::Undercurl, underline_y);
        } else if cell.flags.contains(CellFlags::DOUBLE_UNDERLINE) {
            push(DecorKind::DoubleUnderline, underline_y);
        } else if cell.flags.contains(CellFlags::UNDERLINE) {
            push(DecorKind::Underline, underline_y);
        }

        if cell.flags.contains(CellFlags::STRIKEOUT) {
            push(DecorKind::Strikethrough, y + metrics.strikeout_y as f32);
        }
    }

    /// Rail and wash each visible row that belongs to a command block.
    ///
    /// Keyed on the absolute line id, like [`Self::emit_selection`] — so the
    /// decoration moves with the text under a scroll, compacts through a fold
    /// (the hidden lines are never drawn, so they are never asked about), and
    /// draws a block whose header has scrolled off the top of the viewport
    /// without that being a case anybody had to think about.
    ///
    /// The rail is a fixed 2 physical px rather than a fraction of a cell: it
    /// is a rule, and a rule that thickens with the font stops reading as one.
    #[allow(clippy::too_many_arguments)]
    fn emit_block_bands(
        &mut self,
        grid: &Grid,
        vp: &Viewport<'_>,
        ox: f32,
        oy: f32,
        cw: f32,
        ch: f32,
        clip: [f32; 4],
    ) {
        if vp.blocks.is_empty() {
            return;
        }
        // The rail sits in the gutter, clear of every cell — see `Viewport::gutter`.
        // Its clip has to be widened to match, since the viewport's own rect
        // stops at the grid.
        let reach = (RAIL_PX + RAIL_GAP).min(vp.gutter.max(0.0));
        let rail_x = ox - reach;
        let rail_clip = [clip[0] - reach, clip[1], clip[2] + reach, clip[3]];
        let cols = grid.cols();
        for row in 0..grid.rows() {
            let Some(line) = resolved_row(grid, vp, row).map(|r| r.id) else { continue };
            // Binary search, not a scan: this runs per visible row per frame,
            // and `scrollback` goes to ten million lines — so a linear `find`
            // multiplies the block count by the row count sixty times a
            // second. `zest_core::BlockIndex::block_at` deliberately stays
            // linear for the same shape, and the reason it gives does not
            // apply here: its list has "an open end" on the last entry, while
            // a band's `to` is always a concrete line (a running block's is
            // resolved against the cursor before it ever gets here).
            //
            // Sound because the bands are non-overlapping and ascending —
            // `blocks_are_ascending_and_disjoint` pins that, since it is the
            // precondition rather than a coincidence of construction.
            let i = vp.blocks.partition_point(|b| b.to <= line);
            let Some(band) = vp.blocks.get(i).filter(|b| line >= b.from) else { continue };
            let y = oy + row as f32 * ch;
            if let Some(wash) = band.wash {
                self.rects.push(RectInstance::filled([ox, y, cols as f32 * cw, ch], wash, clip));
            }
            if reach >= RAIL_PX {
                self.rects.push(RectInstance::filled(
                    [rail_x, y, RAIL_PX, ch],
                    band.rail,
                    rail_clip,
                ));
            }
        }
    }

    /// Highlight the selected span on each visible row.
    ///
    /// Only visible rows: the selection may extend far into scrollback, but
    /// there is nothing on screen to paint for those lines. `span_on` is keyed
    /// on the absolute line id, so scrolling moves the highlight with the text
    /// rather than leaving it behind.
    #[allow(clippy::too_many_arguments)]
    fn emit_selection(
        &mut self,
        grid: &Grid,
        vp: &Viewport<'_>,
        ox: f32,
        oy: f32,
        cw: f32,
        ch: f32,
        clip: [f32; 4],
    ) {
        let Some(sel) = vp.selection else { return };
        if sel.is_empty() {
            return;
        }

        let c = vp.selection_bg;
        let fill = LinearRgba::opaque(c.r, c.g, c.b);
        let cols = grid.cols();

        for row in 0..grid.rows() {
            let Some(line) = resolved_row(grid, vp, row).map(|r| r.id) else { continue };
            let Some((from, to)) = sel.span_on(line, cols) else { continue };
            if to <= from {
                continue;
            }
            self.rects.push(RectInstance::filled(
                [
                    ox + from as f32 * cw,
                    oy + row as f32 * ch,
                    (to - from) as f32 * cw,
                    ch,
                ],
                fill,
                clip,
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_cursor(
        &mut self,
        grid: &Grid,
        vp: &Viewport<'_>,
        metrics: CellMetrics,
        ox: f32,
        oy: f32,
        clip: [f32; 4],
        // Where the caret goes while guesses stand: after the last one. The
        // grid's cursor is untouched — input, IME placement and hit testing
        // still read the host's position, exactly as with the trail.
        caret: Option<(usize, usize)>,
    ) {
        // The cursor is hidden while scrolled back -- it refers to a position in
        // the live viewport, not wherever the user happens to be looking.
        if grid.display_offset() != 0 {
            return;
        }

        let c = grid.cursor;
        // In a folded view the cursor's row may sit elsewhere (or, if the
        // fold hid it — which folds of *finished* output never do — nowhere).
        let Some(visual_row) = cursor_visual_row(grid, vp) else { return };
        let (visual_row, col) = match caret {
            Some((r, col)) if vp.row_map.is_none() => (r, col.min(grid.cols().saturating_sub(1))),
            _ => (visual_row, c.col),
        };
        let (cw, ch) = (metrics.cell_w as f32, metrics.cell_h as f32);
        // The trail offset applies to the *focused* caret only. The unfocused
        // hollow box marks where focus would land, and a landmark that drifts
        // is worse than one that does not move at all.
        let x = ox + col as f32 * cw + if vp.focused { vp.cursor_offset[0] } else { 0.0 };
        let y = oy + visual_row as f32 * ch + if vp.focused { vp.cursor_offset[1] } else { 0.0 };
        let color = LinearRgba::opaque(vp.palette.cursor.r, vp.palette.cursor.g, vp.palette.cursor.b);

        if vp.focused {
            if vp.cursor_on {
                self.rects.push(RectInstance::filled(
                    cursor_rect(vp.cursor_shape, [x, y, cw, ch], metrics),
                    color,
                    clip,
                ));
            }
        } else {
            // Unfocused: a hollow box, drawn as four thin rects.
            let t = 1.0f32.max(metrics.underline_thickness as f32);
            for r in [
                [x, y, cw, t],
                [x, y + ch - t, cw, t],
                [x, y, t, ch],
                [x + cw - t, y, t, ch],
            ] {
                self.rects.push(RectInstance::filled(r, color, clip));
            }
        }
    }
}

/// The rectangle a focused cursor of this shape occupies inside its cell.
///
/// Free-standing so the geometry is asserted rather than eyeballed: a bar drawn
/// on the wrong edge or an underline a pixel off the baseline is the kind of
/// thing that looks *nearly* right in a screenshot and wrong in use.
///
/// The thickness for the thin shapes is the font's underline thickness, so the
/// cursor matches the weight of an actual underline at that size rather than
/// being a constant that is fat at 9pt and hairline on a Retina display. At
/// least one physical pixel, or a bar cursor vanishes entirely at small sizes.
fn cursor_rect(shape: CursorShape, cell: [f32; 4], metrics: CellMetrics) -> [f32; 4] {
    let [x, y, cw, ch] = cell;
    let t = 1.0f32.max(metrics.underline_thickness as f32);
    match shape {
        CursorShape::Block => cell,
        // Sits on the bottom edge of the cell rather than on `underline_y`:
        // the cursor marks the *cell*, and an underline cursor floating above
        // the cell boundary reads as a misplaced rule rather than a caret.
        CursorShape::Underline => [x, y + ch - t, cw, t],
        // The leading edge, which for the writing directions this terminal
        // supports is the left one.
        CursorShape::Bar => [x, y, t, ch],
    }
}

/// One shapeable stretch of a row: same face, same colour, no grid oddities.
#[derive(Debug, Default, PartialEq)]
struct Run {
    text: String,
    /// `(byte offset into `text`, the column that character started at)`.
    ///
    /// A shaped cluster reports a byte offset, and the grid needs a column;
    /// this is the only thing that maps between them, which is what places a
    /// ligature at the first of the cells it spans instead of at a byte index.
    starts: Vec<(usize, usize)>,
    style: Style,
    fg: LinearRgba,
}

/// Split a row into runs a shaper can be handed.
///
/// A run breaks wherever the grid stops being a plain sequence of characters,
/// because the shaper knows about none of it:
///
/// * a **style or colour change**, since one shaped run is one face and one
///   instance colour;
/// * a **blank**, which ends a word for shaping as it does for reading;
/// * a **wide cell** and its spacer, whose second column holds no character;
/// * a cell carrying **combining marks**, which live in the row's side table
///   and are drawn separately;
/// * a **hidden** cell.
///
/// Selection is deliberately not a break: it colours backgrounds, and a
/// ligature spanning the edge of a highlight still reads correctly. Nor is the
/// cursor, which is drawn *over* the text rather than in place of it — a caret
/// inside a ligature is a caret on a wide glyph, which is what a caret on a CJK
/// character already is.
///
/// Pure, and separate from the emitting, because this is the part that can be
/// wrong in ways a screenshot will not show: a run that swallows a wide cell
/// puts every glyph after it one column left.
fn breaks_run(cell: &Cell, r: &Row) -> bool {
    is_blank(cell)
        || cell.flags.intersects(CellFlags::WIDE_SPACER | CellFlags::HIDDEN | CellFlags::WIDE)
        || r.extra(cell).is_some_and(|e| !e.zerowidth.is_empty())
}

fn segment_row(r: &Row, cols: usize, palette: &PaletteSnapshot) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut cur = Run::default();
    for col in 0..cols {
        let cell = r.get(col).copied().unwrap_or_default();
        let breaks = breaks_run(&cell, r);
        let style = Style::new(
            cell.flags.contains(CellFlags::BOLD),
            cell.flags.contains(CellFlags::ITALIC),
        );
        let fg = cell_fg(&cell, palette);
        let changed = !cur.text.is_empty() && (style != cur.style || fg != cur.fg);
        if breaks || changed {
            if !cur.text.is_empty() {
                runs.push(std::mem::take(&mut cur));
            }
            cur = Run::default();
        }
        if breaks {
            continue;
        }
        if cur.text.is_empty() {
            cur.style = style;
            cur.fg = fg;
        }
        cur.starts.push((cur.text.len(), col));
        cur.text.push(cell.ch);
    }
    if !cur.text.is_empty() {
        runs.push(cur);
    }
    runs
}

fn is_blank(cell: &Cell) -> bool {
    cell.ch == ' ' || cell.ch == '\0'
}

/// Resolve a colour reference against the palette.
///
/// `Color::Default` needs the caller to say *which* default, because the same
/// sentinel means the foreground in one slot and the background in the other.
fn concrete(color: Color, default: zest_core::Rgb, palette: &PaletteSnapshot) -> zest_core::Rgb {
    match color {
        Color::Default => default,
        Color::Indexed(i) => palette.colors[i as usize],
        Color::Rgb(r, g, b) => zest_core::Rgb::new(r, g, b),
    }
}

/// The background a cell actually paints.
///
/// # Why inverse cannot be handled by swapping `Color` values
///
/// The obvious implementation — "for inverse, use `cell.fg` as the background" —
/// silently loses information, because `Color::Default` is a *reference to a
/// role*, not a value. An inverse cell with a default foreground wants the
/// palette's **foreground** painted as its background; swapping the enums yields
/// `Color::Default` in the background slot, which resolves to the palette
/// background and paints nothing at all. Inverse text then renders invisibly.
///
/// So resolve to concrete colours first, and swap those.
fn cell_bg(cell: &Cell, palette: &PaletteSnapshot, opacity: f32) -> LinearRgba {
    let inverse = cell.flags.contains(CellFlags::INVERSE);
    let (color, default) = if inverse {
        (cell.fg, palette.foreground)
    } else {
        (cell.bg, palette.background)
    };
    let rgb = concrete(color, default, palette);

    // Window opacity applies *only* to a genuinely default background. A cell
    // with an explicit background -- `ls` colours, a TUI panel, an
    // `@sigx/terminal-ui` box -- must stay opaque, and an inverse cell is
    // painting a real colour, not the window's. -> ADR-003.
    let is_window_bg = !inverse && cell.bg == Color::Default;
    let alpha = if is_window_bg { opacity } else { 1.0 };
    LinearRgba::from_srgb(rgb.r, rgb.g, rgb.b, alpha)
}

fn cell_fg(cell: &Cell, palette: &PaletteSnapshot) -> LinearRgba {
    let inverse = cell.flags.contains(CellFlags::INVERSE);
    let (color, default) = if inverse {
        (cell.bg, palette.background)
    } else {
        (cell.fg, palette.foreground)
    };
    let rgb = concrete(color, default, palette);

    // SGR dim is an alpha, not a separate colour. Premultiplication means the
    // instance colour carries it directly.
    let alpha = if cell.flags.contains(CellFlags::DIM) { 0.55 } else { 1.0 };
    LinearRgba::from_srgb(rgb.r, rgb.g, rgb.b, alpha)
}

/// The window's own background, drawn once behind everything.
fn window_bg(palette: &PaletteSnapshot, opacity: f32) -> LinearRgba {
    let c = palette.background;
    LinearRgba::from_srgb(c.r, c.g, c.b, opacity)
}

/// Cells a character occupies, by the same rule the grid uses.
///
/// Kept local rather than reaching for `zest-input`: the renderer draws, it does
/// not encode, and a dependency the other way round would make the layering a
/// cycle waiting to happen.
fn char_cells(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_core::Rgb;

    fn palette() -> PaletteSnapshot {
        let mut p = PaletteSnapshot {
            colors: [Rgb::default(); 256],
            foreground: Rgb::new(0xd7, 0xdc, 0xea),
            background: Rgb::new(0x0b, 0x0f, 0x1a),
            cursor: Rgb::new(0x6e, 0xa8, 0xff),
        };
        p.fill_standard_extended();
        p.colors[1] = Rgb::new(0xe0, 0x60, 0x6a);
        p
    }

    #[test]
    fn the_chrome_boundary_splits_the_buffers_where_chrome_begins() {
        // The renderer draws grid and chrome as split instance ranges out of
        // the same buffers. If the recorded boundary drifts from where the
        // chrome was actually appended, grid glyphs draw after the chrome's
        // panels again — the fleet picker with the shell's prompt shining
        // through it, which is the bug that forced the split.
        let glyph = || GlyphInstance {
            pos: [0.0; 2],
            uv: [0.0; 2],
            size: [1.0; 2],
            color: LinearRgba([0.0; 4]),
            clip: [1.0; 4],
            layer: 0,
            flags: 0,
        };
        let mut scene = Scene::default();
        scene.rects.push(RectInstance::filled([0.0; 4], LinearRgba([0.0; 4]), [0.0; 4]));
        scene.rects.push(RectInstance::filled([1.0; 4], LinearRgba([0.0; 4]), [1.0; 4]));
        scene.glyphs.push(glyph());
        let chrome = Chrome {
            rects: vec![
                RectInstance::filled([2.0; 4], LinearRgba([0.0; 4]), [2.0; 4]),
                RectInstance::filled([3.0; 4], LinearRgba([0.0; 4]), [3.0; 4]),
            ],
            glyphs: vec![glyph(), glyph()],
            // The second chrome rect and glyph belong to the overlay layer.
            overlay_rects_at: 1,
            overlay_glyphs_at: 1,
        };
        scene.append_chrome(&chrome);
        assert_eq!(scene.chrome_rects_at, 2, "chrome rects start after the grid's");
        assert_eq!(scene.chrome_glyphs_at, 1, "chrome glyphs start after the grid's");
        assert_eq!(
            (scene.overlay_rects_at, scene.overlay_glyphs_at),
            (3, 2),
            "the overlay boundary is absolute: chrome start plus the chrome-local split"
        );
        assert_eq!(scene.rects.len(), 4);
        assert_eq!(scene.glyphs.len(), 3);

        scene.clear();
        assert_eq!(
            (scene.chrome_rects_at, scene.chrome_glyphs_at, scene.overlay_rects_at, scene.overlay_glyphs_at),
            (0, 0, 0, 0),
            "a cleared scene must not carry last frame's boundary into this one"
        );
    }

    fn viewport<'a>(grid: &'a Grid, palette: &'a PaletteSnapshot, opacity: f32) -> Viewport<'a> {
        Viewport {
            features: &[],
            ligatures: false,
            rect: [8.0, 8.0, 200.0, 100.0],
            grid,
            palette,
            scroll_px: 0.0,
            cursor_shape: zest_core::CursorShape::Block,
            cursor_offset: [0.0, 0.0],
            focused: true,
            opacity,
            background: None,
            blocks: &[],
            gutter: 0.0,
            selection: None,
            selection_bg: Rgb::new(0x33, 0x44, 0x55),
            preedit: None,
            predicted: None,
            cursor_on: true,
            row_map: None,
        }
    }

    /// The rail rects a build emitted, as `(x, y)`, in order.
    fn rails(scene: &Scene) -> Vec<(f32, f32)> {
        scene
            .rects
            .iter()
            .filter(|r| (r.rect[2] - RAIL_PX).abs() < f32::EPSILON)
            .map(|r| (r.rect[0], r.rect[1]))
            .collect()
    }

    fn band(from: u64, to: u64) -> BlockBand {
        BlockBand { from, to, rail: LinearRgba::opaque(0x40, 0xd0, 0x80), wash: None }
    }

    /// A grid whose visible rows carry line ids `0..rows`.
    fn lined(rows: usize, cols: usize) -> Grid {
        Grid::new(cols, rows, 100)
    }

    #[test]
    fn a_rail_needs_a_gutter_and_never_takes_a_cell() {
        // The constraint the whole design turns on. Chrome paints *over* the
        // glyphs, so a rail drawn a layer up shaves the left edge off column
        // 0 on every row; drawn here, under them, any line starting at column
        // 0 hides it instead. It gets its own space or it does not draw — a
        // rail that comes and goes with the indentation of the output reads as
        // a rendering fault, which is worse than no rail at all.
        let p = palette();
        let grid = lined(4, 10);
        let bands = [band(0, 4)];

        let mut with = Scene::default();
        let vp = Viewport { blocks: &bands, gutter: 16.0, ..viewport(&grid, &p, 1.0) };
        with.emit_block_bands(&grid, &vp, vp.rect[0], vp.rect[1], 8.0, 16.0, vp.rect);
        assert!(!rails(&with).is_empty(), "a gutter is room enough for the rail");
        for (x, _) in rails(&with) {
            assert!(
                x + RAIL_PX <= vp.rect[0],
                "the rail must end before the first column begins, not overlap it"
            );
        }

        let mut without = Scene::default();
        let vp = Viewport { blocks: &bands, gutter: 0.0, ..viewport(&grid, &p, 1.0) };
        without.emit_block_bands(&grid, &vp, vp.rect[0], vp.rect[1], 8.0, 16.0, vp.rect);
        assert!(
            rails(&without).is_empty(),
            "with no padding there is nowhere honest to put it, so it is not drawn"
        );
    }

    #[test]
    fn a_rail_covers_its_lines_and_stops() {
        // Named by line, not by row: the band is asked about whatever the row
        // loop resolved, so a block that ends mid-screen rails exactly its own
        // rows. An open-ended range once railed every blank row below a
        // running command, down to the bottom of the window.
        let p = palette();
        let grid = lined(6, 10);
        let bands = [band(1, 3)];
        let mut scene = Scene::default();
        let vp = Viewport { blocks: &bands, gutter: 16.0, ..viewport(&grid, &p, 1.0) };
        scene.emit_block_bands(&grid, &vp, vp.rect[0], vp.rect[1], 8.0, 16.0, vp.rect);
        assert_eq!(rails(&scene).len(), 2, "lines 1 and 2, and nothing else");
    }

    #[test]
    fn the_band_lookup_finds_the_same_answer_a_scan_would() {
        // `emit_block_bands` binary-searches, which is only correct while the
        // bands are ascending and disjoint. Rather than trust that, check the
        // search against the linear answer for every line in and around the
        // set — including the gaps between blocks, which is where an
        // off-by-one in `partition_point` shows up and nowhere else.
        let bands = [band(0, 3), band(5, 6), band(9, 20)];
        for line in 0..25u64 {
            let scan = bands.iter().find(|b| line >= b.from && line < b.to);
            let i = bands.partition_point(|b| b.to <= line);
            let found = bands.get(i).filter(|b| line >= b.from);
            assert_eq!(
                scan.is_some(),
                found.is_some(),
                "line {line}: the search and a scan must agree"
            );
            if let (Some(a), Some(b)) = (scan, found) {
                assert_eq!(a.from, b.from, "line {line}: and agree on which band");
            }
        }
    }

    #[test]
    fn a_rail_follows_its_lines_through_the_fold_view() {
        // The property that made line ids the right key: `row_map` is how a
        // fold compacts the view, and the rail reads the *resolved* row, so a
        // folded block's hidden lines are never asked about and the rows that
        // remain still rail. Keyed by viewport row this would have been a
        // second fold implementation, drifting from the first.
        let p = palette();
        let grid = lined(6, 10);
        // Draw line 4 where row 0 is, and line 0 where row 1 is: a fold view
        // no plain row range could describe.
        let map = [4usize, 0, usize::MAX, usize::MAX, usize::MAX, usize::MAX];
        let bands = [band(4, 5)];
        let mut scene = Scene::default();
        let vp = Viewport {
            blocks: &bands,
            gutter: 16.0,
            row_map: Some(&map),
            ..viewport(&grid, &p, 1.0)
        };
        scene.emit_block_bands(&grid, &vp, vp.rect[0], vp.rect[1], 8.0, 16.0, vp.rect);
        let rails = rails(&scene);
        assert_eq!(rails.len(), 1, "only the one line the band names is drawn");
        assert_eq!(rails[0].1, vp.rect[1], "and it rails the row that line landed on");
    }

    #[test]
    fn the_bands_are_emitted_before_any_glyph() {
        // Why this lives in the scene at all rather than with the block header
        // chrome: the renderer draws grid rects, then grid glyphs, then chrome.
        // Anything emitted after `chrome_rects_at` paints over the text.
        let p = palette();
        let grid = lined(3, 10);
        let bands = [band(0, 3)];
        let mut scene = Scene::default();
        let vp = Viewport { blocks: &bands, gutter: 16.0, ..viewport(&grid, &p, 1.0) };
        scene.emit_block_bands(&grid, &vp, vp.rect[0], vp.rect[1], 8.0, 16.0, vp.rect);
        scene.append_chrome(&Chrome::default());
        assert!(
            scene.chrome_rects_at >= scene.rects.len(),
            "every band rect must sit below the chrome boundary, or it covers the output"
        );
    }

    #[test]
    fn a_selected_blocks_wash_spans_the_columns_and_stays_translucent() {
        // Under the glyphs but *over* every cell background, so an opaque
        // fill would flatten a TUI's colours to one wash.
        let p = palette();
        let grid = lined(2, 10);
        let wash = LinearRgba([0.05, 0.05, 0.1, 0.1]);
        let bands = [BlockBand { wash: Some(wash), ..band(0, 2) }];
        let mut scene = Scene::default();
        let vp = Viewport { blocks: &bands, gutter: 16.0, ..viewport(&grid, &p, 1.0) };
        scene.emit_block_bands(&grid, &vp, vp.rect[0], vp.rect[1], 8.0, 16.0, vp.rect);
        let washes: Vec<_> =
            scene.rects.iter().filter(|r| (r.rect[2] - 80.0).abs() < f32::EPSILON).collect();
        assert_eq!(washes.len(), 2, "one per line, spanning all ten columns");
        assert!(washes[0].fill.0[3] < 0.2, "a wash that opaque would erase the output");
    }

    #[test]
    fn a_cleared_scene_carries_no_backdrop() {
        // The backdrop is what every pixel outside the viewports gets. Left
        // over from last frame it would paint the padding in the *previous*
        // theme's background for one frame after a theme switch.
        let mut scene = Scene::default();
        assert_eq!(scene.backdrop, LinearRgba::TRANSPARENT, "a fresh scene has none");
        scene.backdrop = LinearRgba::opaque(1, 2, 3);
        scene.clear();
        assert_eq!(scene.backdrop, LinearRgba::TRANSPARENT);
    }

    #[test]
    fn the_backdrop_alone_paints_a_viewport_on_the_same_background() {
        // The regression this guards is the black frame around the terminal:
        // `window.padding` is outside every viewport rect, so the backdrop is
        // the *only* thing that paints it. If the viewport still emitted its own
        // window-background rect on top, `opacity < 1` would composite it twice
        // and the grid would end up less transparent than its own padding.
        let p = palette();
        let grid = Grid::new(4, 2, 0);
        let mut scene = Scene { backdrop: window_bg(&p, 0.8), ..Default::default() };

        scene.push_window_background(&viewport(&grid, &p, 0.8));
        assert!(
            scene.rects.is_empty(),
            "the backdrop already is this background; a second rect would double-blend"
        );
    }

    #[test]
    fn a_viewport_on_its_own_background_still_paints_it() {
        // OSC 11 changes one session's default background, and a split can put
        // two palettes on screen at once — only one of them can be the backdrop.
        let p = palette();
        let mut other = palette();
        other.background = Rgb::new(0x2a, 0x00, 0x00);

        let grid = Grid::new(4, 2, 0);
        let mut scene = Scene { backdrop: window_bg(&p, 1.0), ..Default::default() };

        let vp = viewport(&grid, &other, 1.0);
        scene.push_window_background(&vp);
        assert_eq!(scene.rects.len(), 1, "a background the backdrop does not supply must be drawn");
        assert_eq!(scene.rects[0].rect, vp.rect, "and it covers the viewport, not the window");
        assert_eq!(scene.rects[0].fill, window_bg(&other, 1.0));
    }

    /// A row from text, with an optional style applied to one span.
    fn row_of(text: &str) -> Grid {
        let mut g = Grid::new(text.chars().count().max(1), 1, 0);
        for (i, ch) in text.chars().enumerate() {
            g.row_mut(0).get_mut(i).unwrap().ch = ch;
        }
        g
    }

    #[test]
    fn a_run_is_one_face_one_colour_and_no_grid_oddities() {
        let pal = palette();
        let g = row_of("ab cd");
        let runs = segment_row(g.row(0), 5, &pal);
        assert_eq!(runs.len(), 2, "a blank ends a run: {runs:?}");
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "cd");
        assert_eq!(
            runs[1].starts,
            vec![(0, 3), (1, 4)],
            "the second run remembers the columns it came from, not byte indices"
        );
    }

    #[test]
    fn a_style_change_ends_a_run() {
        // One shaped run is one face. Bold and regular are different faces, so
        // a ligature must not form across the boundary between them.
        let pal = palette();
        let mut g = row_of("abcd");
        for i in 2..4 {
            g.row_mut(0).get_mut(i).unwrap().flags.insert(CellFlags::BOLD);
        }
        let runs = segment_row(g.row(0), 4, &pal);
        assert_eq!(runs.len(), 2, "bold starts a new run: {runs:?}");
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "cd");
    }

    #[test]
    fn a_wide_cell_and_its_spacer_are_never_inside_a_run() {
        // The failure this rules out is silent and total: a run that swallowed
        // the spacer would place every glyph after it one column to the left,
        // and the row would look plausible while being wrong from the CJK
        // character onward.
        let pal = palette();
        // Four columns, not three: a wide character *occupies two*, which is
        // the whole point. Writing it into three silently overwrote the tail
        // and the first version of this test passed for the wrong reason.
        let mut g = row_of("a漢xb");
        g.row_mut(0).get_mut(1).unwrap().flags.insert(CellFlags::WIDE);
        {
            let c = g.row_mut(0).get_mut(2).unwrap();
            c.ch = ' ';
            c.flags.insert(CellFlags::WIDE_SPACER);
        }
        g.row_mut(0).get_mut(3).unwrap().ch = 'b';
        let runs = segment_row(g.row(0), 4, &pal);
        assert_eq!(runs.len(), 2, "the wide cell splits the row: {runs:?}");
        assert_eq!(runs[0].text, "a");
        assert_eq!(runs[1].text, "b");
        assert_eq!(runs[1].starts, vec![(0, 3)], "and the tail keeps its real column");
    }

    #[test]
    fn every_cell_a_run_skips_is_drawn_by_the_other_path() {
        // The bug this pairing exists to prevent, and the one ASCII-only
        // screenshots cannot show: `segment_row` leaves wide cells and
        // mark-carrying cells out of its runs, so if the per-cell path also
        // skips them when shaping is on, they are drawn by *neither* and every
        // CJK character silently disappears.
        //
        // Asserted as a partition: for every column, exactly one of "inside a
        // run" and "breaks a run" is true. The two paths gate on that same
        // predicate, so this is what keeps them exhaustive.
        let pal = palette();
        let mut g = row_of("a漢xb c");
        g.row_mut(0).get_mut(1).unwrap().flags.insert(CellFlags::WIDE);
        {
            let c = g.row_mut(0).get_mut(2).unwrap();
            c.ch = ' ';
            c.flags.insert(CellFlags::WIDE_SPACER);
        }
        let cols = 6;
        let runs = segment_row(g.row(0), cols, &pal);
        let in_a_run: std::collections::BTreeSet<usize> =
            runs.iter().flat_map(|r| r.starts.iter().map(|(_, c)| *c)).collect();

        for col in 0..cols {
            let cell = g.row(0).get(col).copied().unwrap_or_default();
            let breaks = breaks_run(&cell, g.row(0));
            assert_ne!(
                breaks,
                in_a_run.contains(&col),
                "column {col} must be handled by exactly one path, not both or neither"
            );
        }
        assert!(
            breaks_run(&g.row(0).get(1).copied().unwrap(), g.row(0)),
            "the wide cell is the one that has to fall to the per-cell path"
        );
    }

    /// ADR-016: a guess moves the *drawn* caret after itself and nothing
    /// else — the grid's cursor is the host's, and input, IME placement and
    /// hit testing keep reading it. Asserted on the cursor rect's x alone,
    /// because that is the whole visible effect.
    #[test]
    fn the_caret_sits_after_the_guesses_and_the_grid_cursor_stays_put() {
        let p = palette();
        let mut grid = lined(4, 10);
        grid.cursor.row = 1;
        grid.cursor.col = 2;
        let m = CellMetrics {
            cell_w: 10,
            cell_h: 20,
            baseline: 16,
            underline_y: 18,
            underline_thickness: 2,
            strikeout_y: 10,
        };
        let cursor_x = |caret: Option<(usize, usize)>| {
            let mut s = Scene::default();
            let vp = viewport(&grid, &p, 1.0);
            s.emit_cursor(&grid, &vp, m, 0.0, 0.0, vp.rect, caret);
            s.rects.last().expect("a focused cursor is a rect").rect
        };
        assert_eq!(cursor_x(None)[0], 20.0, "no guess: the caret is the grid cursor");
        let with = cursor_x(Some((1, 5)));
        assert_eq!(with[0], 50.0, "three guesses standing: the caret sits after the last");
        assert_eq!(with[1], 20.0, "...on the same row");
        assert_eq!((grid.cursor.row, grid.cursor.col), (1, 2), "the grid's cursor is untouched");
        assert_eq!(
            cursor_x(Some((1, 99)))[0],
            90.0,
            "a caret past the edge clamps to the last column rather than leaving the pane"
        );
    }

    #[test]
    fn an_empty_row_shapes_nothing() {
        let pal = palette();
        let g = Grid::new(8, 1, 0);
        assert!(segment_row(g.row(0), 8, &pal).is_empty(), "blanks alone are not a run");
    }

    #[test]
    fn each_cursor_shape_draws_where_it_should() {
        // The renderer drew a filled block for every cursor, whatever the
        // style said -- so `cursor.shape` did nothing *and* DECSCUSR was
        // ignored, on every platform and every transport. Geometry asserted
        // rather than eyeballed: a bar on the wrong edge or an underline a
        // pixel off the cell boundary looks nearly right in a screenshot and
        // wrong in use.
        let m = CellMetrics {
            cell_w: 10,
            cell_h: 20,
            baseline: 16,
            underline_y: 18,
            underline_thickness: 2,
            strikeout_y: 10,
        };
        let cell = [100.0, 200.0, 10.0, 20.0];

        assert_eq!(cursor_rect(CursorShape::Block, cell, m), cell, "a block fills its cell");
        assert_eq!(
            cursor_rect(CursorShape::Underline, cell, m),
            [100.0, 218.0, 10.0, 2.0],
            "an underline sits on the cell's bottom edge, full width"
        );
        assert_eq!(
            cursor_rect(CursorShape::Bar, cell, m),
            [100.0, 200.0, 2.0, 20.0],
            "a bar sits on the leading edge, full height"
        );
    }

    #[test]
    fn a_thin_cursor_never_vanishes() {
        // The thickness follows the font's underline so the caret matches the
        // weight of a real underline at that size -- but a face reporting 0 at
        // a small size would otherwise produce a zero-width bar, which is a
        // cursor that is simply not there.
        let m = CellMetrics {
            cell_w: 6,
            cell_h: 12,
            baseline: 9,
            underline_y: 11,
            underline_thickness: 0,
            strikeout_y: 6,
        };
        let cell = [0.0, 0.0, 6.0, 12.0];
        assert_eq!(cursor_rect(CursorShape::Bar, cell, m)[2], 1.0, "at least one physical pixel");
        assert_eq!(cursor_rect(CursorShape::Underline, cell, m)[3], 1.0);
    }

    #[test]
    fn a_picture_replaces_the_window_background_rather_than_joining_it() {
        // The pair of assertions that keeps the two layers from both painting:
        // the picture goes in, and the rect that would have blended over it
        // (and composited the opacity a second time) does not.
        let p = palette();
        let grid = Grid::new(4, 2, 0);
        let mut scene = Scene { backdrop: window_bg(&p, 0.8), ..Default::default() };
        let mut vp = viewport(&grid, &p, 0.8);
        vp.background = Some(BackgroundImage {
            image: crate::image::ImageId(7),
            size: [10, 10],
            fit: crate::image::BackgroundFit::Fill,
            dim: 0.25,
        });

        scene.push_window_background(&vp);

        assert_eq!(scene.images.len(), 1, "the pane's background is the picture");
        assert!(scene.rects.is_empty(), "a rect over it would composite opacity twice");
        assert_eq!(scene.images[0].id(), crate::image::ImageId(7));
        assert_eq!(
            scene.images[0].base,
            window_bg(&p, 0.8),
            "the quad carries the background it replaced, or `dim = 1` is not a no-op"
        );
    }

    #[test]
    fn a_picture_is_drawn_even_when_the_clear_already_is_the_background() {
        // The rect is skipped in that case, deliberately. The picture must not
        // be: skipping it is a pane that silently loses its wallpaper whenever
        // it happens to be the window's own palette, which is the usual case.
        let p = palette();
        let grid = Grid::new(4, 2, 0);
        let mut scene = Scene { backdrop: window_bg(&p, 1.0), ..Default::default() };
        let mut vp = viewport(&grid, &p, 1.0);

        scene.push_window_background(&vp);
        assert!(scene.rects.is_empty() && scene.images.is_empty(), "nothing to draw without one");

        vp.background = Some(BackgroundImage {
            image: crate::image::ImageId(1),
            size: [10, 10],
            fit: crate::image::BackgroundFit::Fit,
            dim: 0.0,
        });
        scene.push_window_background(&vp);
        assert_eq!(scene.images.len(), 1, "the picture is drawn regardless of the clear");
    }

    #[test]
    fn a_default_background_cell_still_emits_nothing_over_a_picture() {
        // This is the whole feature: the picture is visible through default
        // cells because they draw no rect, and hidden behind explicit ones
        // because they do. Both halves, on the same row.
        let p = palette();
        let mut grid = Grid::new(4, 1, 0);
        grid.row_mut(0).get_mut(0).unwrap().bg = Color::Rgb(0x80, 0x00, 0x00);
        let mut scene = Scene::default();
        let mut vp = viewport(&grid, &p, 1.0);
        vp.background = Some(BackgroundImage {
            image: crate::image::ImageId(2),
            size: [4, 4],
            fit: crate::image::BackgroundFit::Fill,
            dim: 0.0,
        });

        scene.push_window_background(&vp);
        assert!(scene.rects.is_empty(), "the pane's own background is the picture");

        let r = vp.rect;
        scene.emit_row_backgrounds(&grid, 0, &vp, r[0], r[1], 10.0, 20.0, r);

        assert_eq!(
            scene.rects.len(),
            1,
            "only the explicitly coloured cell paints; the other three let the picture through"
        );
        assert_eq!(
            scene.rects[0].rect[2], 10.0,
            "and it is one cell wide, not the whole row"
        );
    }

    #[test]
    fn window_opacity_applies_only_to_default_backgrounds() {
        let p = palette();

        let plain = cell_bg(&Cell::default(), &p, 0.8);
        assert!((plain.0[3] - 0.8).abs() < 1e-5, "the window background is translucent");

        // The bug this guards: applying opacity everywhere double-darkens and
        // makes TUI panels see-through, so every sigx TUI would look broken.
        let explicit = Cell { bg: Color::Indexed(1), ..Default::default() };
        assert!(
            (cell_bg(&explicit, &p, 0.8).0[3] - 1.0).abs() < 1e-5,
            "an explicit background stays opaque"
        );

        let truecolor = Cell { bg: Color::Rgb(10, 20, 30), ..Default::default() };
        assert!((cell_bg(&truecolor, &p, 0.2).0[3] - 1.0).abs() < 1e-5);
    }

    /// The bug this guards rendered inverse text completely invisibly.
    ///
    /// Swapping the `Color` enums looks right and is wrong: `Color::Default` is
    /// a reference to a *role*, so an inverse cell with a default foreground
    /// swapped to `Color::Default` in the background slot, resolved to the
    /// palette background, and painted nothing.
    #[test]
    fn inverse_with_default_colours_paints_the_foreground() {
        let p = palette();
        let cell = Cell { flags: CellFlags::INVERSE, ..Default::default() };

        let bg = cell_bg(&cell, &p, 1.0);
        let expected_bg = LinearRgba::opaque(p.foreground.r, p.foreground.g, p.foreground.b);
        assert_eq!(bg.0, expected_bg.0, "inverse paints the FOREGROUND as its background");
        assert_ne!(bg, window_bg(&p, 1.0), "and so must not be skipped as a default run");

        let fg = cell_fg(&cell, &p);
        let expected_fg = LinearRgba::opaque(p.background.r, p.background.g, p.background.b);
        assert_eq!(fg.0, expected_fg.0, "and draws text in the background colour");
    }

    #[test]
    fn inverse_is_opaque_even_with_window_transparency() {
        // Inverse paints a real colour, not the window's, so opacity must not
        // apply to it.
        let p = palette();
        let cell = Cell { flags: CellFlags::INVERSE, ..Default::default() };
        assert!((cell_bg(&cell, &p, 0.3).0[3] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn inverse_with_explicit_colours_swaps_them() {
        let p = palette();
        let cell = Cell {
            fg: Color::Indexed(1),
            bg: Color::Rgb(9, 9, 9),
            flags: CellFlags::INVERSE,
            ..Default::default()
        };
        let c1 = p.colors[1];
        assert_eq!(cell_bg(&cell, &p, 1.0).0, LinearRgba::opaque(c1.r, c1.g, c1.b).0);
        assert_eq!(cell_fg(&cell, &p).0, LinearRgba::opaque(9, 9, 9).0);
    }

    #[test]
    fn dim_is_expressed_as_alpha() {
        let p = palette();
        let cell = Cell { flags: CellFlags::DIM, ..Default::default() };
        let fg = cell_fg(&cell, &p);
        assert!((fg.0[3] - 0.55).abs() < 1e-5);
        // Premultiplied, so the colour channels are scaled too.
        let full = cell_fg(&Cell::default(), &p);
        assert!(fg.0[0] < full.0[0]);
    }

    #[test]
    fn truecolor_bypasses_the_palette() {
        let p = palette();
        let cell = Cell { fg: Color::Rgb(1, 2, 3), ..Default::default() };
        assert_eq!(cell_fg(&cell, &p).0, LinearRgba::from_srgb(1, 2, 3, 1.0).0);
    }

    #[test]
    fn wide_spacers_and_hidden_cells_emit_no_glyph() {
        // Both are skipped in emit_row_glyphs; assert the predicates that drive
        // it, since the emit path needs a GPU.
        let spacer = Cell { flags: CellFlags::WIDE_SPACER, ..Default::default() };
        assert!(spacer.flags.contains(CellFlags::WIDE_SPACER));

        let hidden = Cell { ch: 'x', flags: CellFlags::HIDDEN, ..Default::default() };
        assert!(hidden.flags.contains(CellFlags::HIDDEN));
        assert!(!is_blank(&hidden), "hidden is skipped for a different reason than blankness");
    }

    #[test]
    fn scene_clear_keeps_capacity() {
        // Rebuilding 30k instances per frame must not reallocate.
        let mut s = Scene::default();
        s.rects.push(RectInstance::filled([0.0; 4], LinearRgba::TRANSPARENT, [0.0; 4]));
        let cap = s.rects.capacity();
        s.clear();
        assert!(s.rects.is_empty());
        assert_eq!(s.rects.capacity(), cap);
    }
}
