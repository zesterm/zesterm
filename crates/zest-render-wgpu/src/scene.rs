//! Building instance lists from a terminal grid.
//!
//! The renderer takes a slice of viewports from day one, even though M1 always
//! passes exactly one. Panes then cost a loop and a clip rect rather than a
//! restructuring of the render loop. → ADR-003 / ROADMAP sequencing.

use zest_core::grid::Row;
use zest_core::{Cell, CellFlags, Color, Grid, PaletteSnapshot};
use zest_font::{CellMetrics, Fonts, GlyphKey, Style};

use crate::atlas::{Atlas, Cached};
use crate::instance::{
    glyph_flags, DecorInstance, DecorKind, GlyphInstance, LinearRgba, RectInstance,
};

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
    /// Blink phase: `false` hides the focused block cursor (the off half of
    /// the cycle). The hollow unfocused cursor never blinks — it marks where
    /// focus would land, and a vanishing landmark is worse than none.
    pub cursor_on: bool,
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
}

/// Everything to draw this frame.
#[derive(Debug, Default)]
pub struct Scene {
    pub rects: Vec<RectInstance>,
    pub glyphs: Vec<GlyphInstance>,
    pub decors: Vec<DecorInstance>,
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
}

impl Scene {
    /// Reuse the allocations. Called every frame; allocating fresh vectors for
    /// 30k instances per frame would show up in the profile.
    pub fn clear(&mut self) {
        self.rects.clear();
        self.glyphs.clear();
        self.decors.clear();
        self.grid_origin = [0.0, 0.0];
        self.chrome_rects_at = 0;
        self.chrome_glyphs_at = 0;
    }

    /// Build a frame.
    ///
    /// Rasterizes any glyph the atlas is missing, which is why this needs
    /// `Fonts` and the GPU queue. In steady state there are no misses at all.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut Atlas,
        fonts: &mut Fonts,
        metrics: CellMetrics,
        viewports: &[Viewport<'_>],
        chrome: &Chrome,
    ) {
        self.clear();

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
        self.rects.extend_from_slice(&chrome.rects);
        self.glyphs.extend_from_slice(&chrome.glyphs);
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

        // The window background. One instance rather than one per blank cell.
        self.rects.push(RectInstance::filled(vp.rect, window_bg(vp.palette, vp.opacity), clip));

        for row in 0..grid.rows() {
            let y = oy + row as f32 * ch;
            self.emit_row_backgrounds(grid, row, vp, ox, y, cw, ch, clip);
        }

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
            self.emit_cursor(grid, vp, metrics, ox, oy, clip);
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

            if !is_blank(&cell) {
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

    fn emit_cursor(
        &mut self,
        grid: &Grid,
        vp: &Viewport<'_>,
        metrics: CellMetrics,
        ox: f32,
        oy: f32,
        clip: [f32; 4],
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
        let (cw, ch) = (metrics.cell_w as f32, metrics.cell_h as f32);
        let x = ox + c.col as f32 * cw;
        let y = oy + visual_row as f32 * ch;
        let color = LinearRgba::opaque(vp.palette.cursor.r, vp.palette.cursor.g, vp.palette.cursor.b);

        if vp.focused {
            if vp.cursor_on {
                self.rects.push(RectInstance::filled([x, y, cw, ch], color, clip));
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
            rects: vec![RectInstance::filled([2.0; 4], LinearRgba([0.0; 4]), [2.0; 4])],
            glyphs: vec![glyph(), glyph()],
        };
        scene.append_chrome(&chrome);
        assert_eq!(scene.chrome_rects_at, 2, "chrome rects start after the grid's");
        assert_eq!(scene.chrome_glyphs_at, 1, "chrome glyphs start after the grid's");
        assert_eq!(scene.rects.len(), 3);
        assert_eq!(scene.glyphs.len(), 3);

        scene.clear();
        assert_eq!(
            (scene.chrome_rects_at, scene.chrome_glyphs_at),
            (0, 0),
            "a cleared scene must not carry last frame's boundary into this one"
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
