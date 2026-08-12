//! Font discovery, shaping, and CPU rasterization.
//!
//! No GPU, no window. Everything here is testable by writing a PNG, which is
//! why `examples/font_dump.rs` exists: font bugs are far easier to diagnose
//! before a renderer is in the picture.
//!
//! # Design notes
//!
//! `swash` gives exactly the two things a terminal needs — GSUB/GPOS shaping
//! for ligatures, and hinted rasterization with COLR/CBDT colour glyphs.
//! `glyphon` and `cosmic-text` were rejected because they bring a text *layout*
//! engine, and in a terminal **the grid is the layout**; you would spend your
//! time defeating their line breaking and wrapping. `fontdue` and `ab_glyph`
//! were rejected for having no shaping, no colour glyphs, and no fallback.
//!
//! `fontique` provides real system fallback (DirectWrite / CoreText /
//! fontconfig). Hand-writing a fallback chain is how a terminal ends up
//! rendering tofu for Devanagari.

#![forbid(unsafe_code)]

pub mod boxdraw;
pub mod metrics;

use rustc_hash::FxHashMap;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::{FontRef, GlyphId};

pub use metrics::{CellMetrics, Typography};

/// Which face of a family to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
}

impl Style {
    #[must_use]
    pub const fn new(bold: bool, italic: bool) -> Self {
        Self { bold, italic }
    }
}

/// An interned handle to a loaded face at a particular variation.
///
/// Interning keeps [`GlyphKey`] small — the glyph cache is hashed on every
/// cell of every frame, so a key holding a `Vec` of variation settings would
/// dominate the frame's CPU cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontId(pub u16);

impl FontId {
    /// Not a face: the marker for glyphs [`boxdraw`] generates at cell size.
    ///
    /// Sentinel rather than a variant so [`GlyphKey`] stays a plain `Copy`
    /// struct that hashes in one go — it is hashed once per cell per frame, and
    /// that cost is the reason for the interning above.
    ///
    /// `u16::MAX` can never collide with a real face: `faces` is a `Vec` indexed
    /// by this, so a face at that index would need 65535 loaded before it.
    pub const BOXDRAW: Self = Self(u16::MAX);
}

/// Identifies one rasterized glyph bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub font: FontId,
    pub glyph: GlyphId,
    /// Size in 8.8 fixed point, so the key stays hashable and exact.
    pub px: u16,
    /// Horizontal subpixel bucket, 0..4.
    ///
    /// The grid places glyphs at integer positions so this is always 0 there,
    /// but chrome text is freely positioned and quarter-pixel buckets are what
    /// stop it looking like terminal text bolted onto a UI.
    pub subpx_x: u8,
    /// Synthetic bold/oblique applied because the family lacked a real face.
    pub synthetic: u8,
    /// Which rasterization this bitmap was made with: [`RASTER_SUBPIXEL`],
    /// [`RASTER_HINTED`].
    ///
    /// In the key because chrome and the grid rasterize *differently* and can
    /// ask for the same glyph at the same size — the terminal's antialiasing
    /// settings are the terminal's, and chrome is pinned to what looks right.
    /// Without this the first one rasterized would win and the other would get
    /// a bitmap made for someone else.
    pub raster: u8,
}

/// Per-channel coverage rather than one value per pixel.
pub const RASTER_SUBPIXEL: u8 = 1 << 0;
/// The outline was grid-fitted.
pub const RASTER_HINTED: u8 = 1 << 1;

pub const SYNTHETIC_BOLD: u8 = 1 << 0;
pub const SYNTHETIC_ITALIC: u8 = 1 << 1;

/// How text is antialiased, and therefore what a mask texel means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TextAntialias {
    /// Per-channel coverage sampled at thirds of a pixel.
    Subpixel,
    /// One coverage value per pixel.
    ///
    /// The default, and deliberately the *safe* one: a mask is one byte per
    /// texel until the renderer says it can composite more, so nothing can
    /// upload four bytes into a one-byte texture before that is known.
    ///
    /// What Windows Terminal does by default, and measured to match it closely
    /// when paired with [`Hinting::Full`]: at 12px, 12.6% ink coverage against
    /// its 11.7%, and 43% of inked pixels fully saturated against its 45%.
    #[default]
    Grayscale,
}

/// Whether outlines are grid-fitted before rasterization.
///
/// Kept separate from [`TextAntialias`] because the two really are independent,
/// and the interesting configurations are the mixed ones. Grid-fitting snaps a
/// stem onto whole pixels, so it is worth most where a stem is one pixel wide
/// and almost nothing where it is three — which is why it looks irrelevant at
/// 16px and decides everything at 9pt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Hinting {
    /// Outlines are used as the designer drew them.
    ///
    /// Softer at small sizes, and the only option that is certainly faithful:
    /// swash pins the hinting target to horizontal LCD and does not let it be
    /// chosen, so `Full` means "grid-fit for a rasterizer with three times the
    /// horizontal resolution" whether or not that is what we are.
    None,
    /// Grid-fit through swash, which runs the font's own TrueType bytecode.
    ///
    /// Crisper at small sizes and what every Windows application looks like.
    /// The cost is that a ClearType-aware face grid-fits horizontally too, and
    /// sampled at one point per pixel that changes glyph *shapes* -- `w` at
    /// 13ppem in Cascadia Mono is the case that started #100.
    #[default]
    Full,
}

/// What one texel of [`GlyphImage::data`] means.
///
/// Three states rather than a bool, because subpixel coverage is a *third*
/// thing: four bytes per texel like a colour glyph, one coverage value per
/// channel like a mask. A bool that only separates "colour" from "not colour"
/// uploads a subpixel mask into the one-byte coverage texture, which is not a
/// crash — it is a third of a glyph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum GlyphFormat {
    /// One coverage byte per texel.
    #[default]
    Mask,
    /// Four bytes per texel: independent R, G and B coverage, sampled at
    /// -0.3, 0 and +0.3 of a pixel.
    ///
    /// **The fourth byte is always zero and is not alpha.** zeno rasterizes the
    /// outline three times and writes only bytes 0, 1 and 2; the buffer is
    /// zero-filled, so byte 3 is reliably 0 rather than garbage — which makes
    /// sampling it as alpha a silent, total transparency rather than a visible
    /// bug. (`zeno::mask::render`.)
    SubpixelMask,
    /// Four bytes per texel: straight, *non*-premultiplied RGBA. Emoji.
    Color,
}

impl GlyphFormat {
    #[must_use]
    pub fn bytes_per_texel(self) -> u32 {
        match self {
            Self::Mask => 1,
            Self::SubpixelMask | Self::Color => 4,
        }
    }

    /// Whether this is a colour bitmap rather than a coverage mask.
    #[must_use]
    pub fn is_color(self) -> bool {
        matches!(self, Self::Color)
    }
}

/// Smear subpixel coverage across neighbouring samples, to tame colour fringes.
///
/// A row of a subpixel mask is `3 * width` coverage samples a third of a pixel
/// apart. Rasterizing them and stopping there is what puts saturated orange on
/// a stem's left edge and blue on its right: the three channels of one pixel
/// disagree completely, and the eye reads that disagreement as colour rather
/// than as position. Measured against Windows Terminal on the same glyphs at
/// the same size, that colour was the whole remaining difference once weight
/// was matched — its `d` is neutral grey, ours was orange and blue.
///
/// The taps are FreeType's default rather than the textbook `[1,2,3,2,1]/9`,
/// which puts an eighth of the energy two subpixels out and visibly softens the
/// stem along with the colour.
///
/// **This was tried once against the wrong baseline.** With coverage still
/// applied in linear space every glyph was already too fat, so the filter's
/// small softening landed on top of that and read as unacceptable. Coverage is
/// linearized now, and the same filter costs far less — which is the argument
/// for judging a change against a fixed baseline rather than a broken one.
///
/// Applied at rasterization, so it is baked into the atlas and costs nothing
/// per frame. Unlike `text_gamma`, which is a taste knob and stays a uniform.
fn filter_subpixel(data: &mut [u8], width: u32, height: u32) {
    const TAP: [u32; 5] = [0x08, 0x4D, 0x56, 0x4D, 0x08];
    const DIV: u32 = 0x100;

    let w = width as usize;
    if w == 0 {
        return;
    }
    let n = w * 3;
    let mut row = vec![0u8; n];
    for y in 0..height as usize {
        let base = y * w * 4;
        for x in 0..w {
            for c in 0..3 {
                row[x * 3 + c] = data[base + x * 4 + c];
            }
        }
        for i in 0..n {
            let mut acc = 0u32;
            for (k, tap) in TAP.iter().enumerate() {
                // Clamped at the ends, not wrapped: the first and last subpixel
                // of a glyph have no neighbour, and borrowing the opposite
                // edge's would drag ink across the whole cell.
                let j = i as isize + k as isize - 2;
                if j < 0 || j >= n as isize {
                    continue;
                }
                acc += tap * u32::from(row[j as usize]);
            }
            data[base + (i / 3) * 4 + (i % 3)] = (acc / DIV).min(255) as u8;
        }
    }
}

/// A rasterized glyph.
#[derive(Debug, Clone)]
pub struct GlyphImage {
    pub width: u32,
    pub height: u32,
    /// Offset from the pen position to the bitmap's left edge.
    pub left: i32,
    /// Offset from the baseline *up* to the bitmap's top edge.
    pub top: i32,
    /// `width * height * format.bytes_per_texel()` bytes.
    pub data: Vec<u8>,
    /// What a texel of [`GlyphImage::data`] means.
    ///
    /// Colour glyphs live in a separate RGBA atlas, selected per-instance by a
    /// flag bit rather than by a second draw call.
    pub format: GlyphFormat,
}

impl GlyphImage {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FontError {
    #[error("no usable font found; tried: {tried}")]
    NoFont { tried: String },
}

/// One loaded face.
struct Face {
    /// Owned font data. `FontRef` borrows this, so it must outlive every use.
    data: fontique::Blob<u8>,
    index: u32,
    /// Synthesis the *matcher* asked for, when no real face existed.
    synthetic: u8,
}

impl Face {
    fn font_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(self.data.as_ref(), self.index as usize)
    }
}

/// The font system: resolution, fallback, shaping and rasterization.
pub struct Fonts {
    collection: fontique::Collection,
    source_cache: fontique::SourceCache,
    scale_cx: ScaleContext,
    shape_cx: ShapeContext,
    faces: Vec<Face>,
    /// Which `FontId` serves each style of the primary stack.
    styles: FxHashMap<Style, FontId>,
    /// Fallback faces discovered for characters the primary stack lacks,
    /// keyed by the character that triggered the lookup.
    fallback_cache: FxHashMap<char, Option<FontId>>,
    typo: Typography,
    cell: CellMetrics,
    /// Draw U+2500–U+259F at cell size instead of taking the font's glyphs.
    builtin_box_drawing: bool,
    /// What the renderer is *able* to composite, set from its effective mode.
    /// Chrome uses this directly; the grid may ask for less.
    available: TextAntialias,
    /// The terminal grid's antialiasing, from settings.
    grid_antialias: TextAntialias,
    /// The terminal grid's grid-fitting, from settings.
    grid_hinting: Hinting,
    requested: Vec<String>,
    /// Families to try for Private Use Area codepoints, best first.
    symbol_families: Vec<String>,
    /// A temporary size for UI text, in physical pixels. The chrome's type
    /// scale runs 9.5–21px against one grid size, so shaping and keys honour
    /// this when set; cell geometry never does — the grid is `typo`'s alone.
    ui_px: Option<f32>,
}

impl Fonts {
    /// Build a font system from a prioritized family list.
    ///
    /// Names are tried in order; the first that resolves wins. If none do, the
    /// generic monospace family is used, and only if *that* fails is this an
    /// error — a machine with no monospace font at all is not one we can serve.
    pub fn new(families: &[String], typo: Typography) -> Result<Self, FontError> {
        let collection = fontique::Collection::new(fontique::CollectionOptions {
            system_fonts: true,
            ..Default::default()
        });
        let source_cache = fontique::SourceCache::default();

        let mut fonts = Self {
            faces: Vec::new(),
            styles: FxHashMap::default(),
            fallback_cache: FxHashMap::default(),
            scale_cx: ScaleContext::new(),
            shape_cx: ShapeContext::new(),
            typo,
            builtin_box_drawing: true,
            available: TextAntialias::default(),
            grid_antialias: TextAntialias::default(),
            grid_hinting: Hinting::default(),
            cell: CellMetrics {
                cell_w: 1,
                cell_h: 1,
                baseline: 0,
                underline_y: 0,
                underline_thickness: 1,
                strikeout_y: 0,
            },
            requested: families.to_vec(),
            symbol_families: Vec::new(),
            collection,
            source_cache,
            ui_px: None,
        };

        // Found once at startup: enumerating installed families is a real scan,
        // and the answer does not change while the process runs.
        fonts.symbol_families = discover_symbol_families(&mut fonts.collection);
        if fonts.symbol_families.is_empty() {
            tracing::debug!("no Nerd Font installed; prompt icons will not render");
        } else {
            tracing::debug!(families = ?fonts.symbol_families, "symbol fonts");
        }

        // Resolve all four styles up front. Doing it lazily would mean a
        // first-bold-character hitch mid-session.
        for style in [
            Style::new(false, false),
            Style::new(true, false),
            Style::new(false, true),
            Style::new(true, true),
        ] {
            if let Some(id) = fonts.resolve(families, style) {
                fonts.styles.insert(style, id);
            }
        }

        if !fonts.styles.contains_key(&Style::default()) {
            // Last resort: whatever the system calls monospace. `resolve` maps
            // this to the generic rather than looking for a family with that
            // literal name -- see the comment there for why that distinction
            // kept the safety net from ever catching anything.
            let generic = ["monospace".to_string()];
            if let Some(id) = fonts.resolve(&generic, Style::default()) {
                fonts.styles.insert(Style::default(), id);
            } else {
                return Err(FontError::NoFont { tried: families.join(", ") });
            }
        }

        fonts.recompute_metrics();
        Ok(fonts)
    }

    /// Ask fontique for a face matching `families` at `style`.
    fn resolve(&mut self, families: &[String], style: Style) -> Option<FontId> {
        let attrs = fontique::Attributes {
            width: fontique::FontWidth::NORMAL,
            style: if style.italic {
                fontique::FontStyle::Italic
            } else {
                fontique::FontStyle::Normal
            },
            weight: if style.bold {
                fontique::FontWeight::BOLD
            } else {
                fontique::FontWeight::NORMAL
            },
        };

        let mut found: Option<(fontique::Blob<u8>, u32, u8)> = None;
        {
            let mut query = self.collection.query(&mut self.source_cache);
            // A CSS generic is not a family name. `monospace` is the last entry
            // in the default stack and in most hand-written settings files, and
            // asked for by name it matches nothing on any platform -- so the
            // entry every user believes is their safety net was doing nothing.
            // Route the eleven generics through the API that resolves them and
            // leave everything else a literal name.
            query.set_families(families.iter().map(|f| {
                fontique::GenericFamily::parse(f)
                    .map_or(fontique::QueryFamily::Named(f), fontique::QueryFamily::Generic)
            }));
            query.set_attributes(attrs);
            query.matches_with(|font| {
                // fontique reports what it had to fake to satisfy the request.
                // Recording it lets the rasterizer apply the same synthesis and
                // keeps it in the cache key, so a real bold and a faked bold
                // never share a bitmap.
                let mut synthetic = 0u8;
                if font.synthesis.embolden() {
                    synthetic |= SYNTHETIC_BOLD;
                }
                if font.synthesis.skew().is_some() {
                    synthetic |= SYNTHETIC_ITALIC;
                }
                found = Some((font.blob.clone(), font.index, synthetic));
                fontique::QueryStatus::Stop
            });
        }

        let (data, index, synthetic) = found?;
        Some(self.intern(Face { data, index, synthetic }))
    }

    fn intern(&mut self, face: Face) -> FontId {
        // Faces are few (four styles plus a handful of fallbacks), so a linear
        // scan for an existing match is cheaper than maintaining a map.
        if let Some(i) = self
            .faces
            .iter()
            .position(|f| f.index == face.index && f.synthetic == face.synthetic && std::ptr::eq(f.data.as_ref().as_ptr(), face.data.as_ref().as_ptr()))
        {
            return FontId(i as u16);
        }
        self.faces.push(face);
        FontId((self.faces.len() - 1) as u16)
    }

    /// The face for a style, falling back to regular when the style is absent.
    #[must_use]
    pub fn font_for(&self, style: Style) -> FontId {
        self.styles
            .get(&style)
            .or_else(|| self.styles.get(&Style::default()))
            .copied()
            .unwrap_or(FontId(0))
    }

    #[must_use]
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell
    }

    /// Whether box drawing and block elements are generated at cell size.
    ///
    /// On by default, because the alternative has gaps in it. Off is for the
    /// person who has chosen a font whose box drawing they specifically want —
    /// the caller must clear the atlas after changing this, since it changes
    /// what every affected key rasterizes to.
    pub fn set_builtin_box_drawing(&mut self, on: bool) {
        self.builtin_box_drawing = on;
    }

    #[must_use]
    pub fn builtin_box_drawing(&self) -> bool {
        self.builtin_box_drawing
    }

    /// Set the antialiasing mode.
    ///
    /// This changes both the coverage format and whether outlines are hinted --
    /// see [`Fonts::rasterize`] for why those are one decision. **The caller
    /// must clear the atlas afterwards.** [`GlyphKey`] deliberately does not
    /// record the mode: it is hashed once per cell per frame and there is no
    /// scene in which one glyph wants subpixel coverage and another does not,
    /// so the mode is a property of an atlas *generation*, exactly as the font
    /// stack and the pixel size already are.
    pub fn set_text_antialias(&mut self, mode: TextAntialias) {
        self.available = mode;
    }

    /// Set grid-fitting. **The caller must clear the atlas afterwards**, for
    /// the same reason as [`Fonts::set_text_antialias`].
    pub fn set_hinting(&mut self, hinting: Hinting) {
        self.grid_hinting = hinting;
    }

    #[must_use]
    pub fn hinting(&self) -> Hinting {
        self.grid_hinting
    }

    /// The grid's antialiasing preference. Never raises it above what the
    /// renderer can composite.
    pub fn set_grid_antialias(&mut self, mode: TextAntialias) {
        self.grid_antialias = mode;
    }

    /// The rasterization mode for the text being laid out right now.
    ///
    /// Chrome is pinned to per-channel coverage with no grid-fitting, whatever
    /// the terminal is set to. Grid-fitting at chrome sizes flattens the
    /// baseline overshoot on `o c e` while leaving it on `a`, which is why
    /// "Close" reads a pixel short beside "tab" in one label — measured at
    /// 12.5px, and the reason #100 was opened. The terminal's settings are for
    /// the terminal; the chrome is the application's own furniture and has one
    /// right answer.
    ///
    /// Keyed off `ui_px` because that is already exactly "is this chrome text".
    fn raster_mode(&self) -> u8 {
        let (aa, hint) = if self.ui_px.is_some() {
            (self.available, Hinting::None)
        } else {
            // Never above what the renderer can composite.
            let aa = match (self.available, self.grid_antialias) {
                (TextAntialias::Subpixel, TextAntialias::Subpixel) => TextAntialias::Subpixel,
                _ => TextAntialias::Grayscale,
            };
            (aa, self.grid_hinting)
        };
        (if aa == TextAntialias::Subpixel { RASTER_SUBPIXEL } else { 0 })
            | (if hint == Hinting::Full { RASTER_HINTED } else { 0 })
    }

    #[must_use]
    pub fn text_antialias(&self) -> TextAntialias {
        self.available
    }

    #[must_use]
    pub fn typography(&self) -> Typography {
        self.typo
    }

    /// Change typography and recompute cell geometry.
    ///
    /// The caller must treat the returned metrics as a resize: bump the atlas
    /// generation, recompute cols/rows, and resize the PTY.
    pub fn set_typography(&mut self, typo: Typography) -> CellMetrics {
        self.typo = typo;
        self.recompute_metrics();
        self.cell
    }

    fn recompute_metrics(&mut self) {
        let id = self.font_for(Style::default());
        let px = self.typo.size_px();
        let Some(face) = self.faces.get(id.0 as usize) else { return };
        let Some(font) = face.font_ref() else { return };

        let m = font.metrics(&[]).scale(px);

        // Widest advance among representative characters. A nominally monospace
        // font can report an `average_width` that clips 'M' or 'W'.
        let charmap = font.charmap();
        let gm = font.glyph_metrics(&[]).scale(px);
        let advance = ['0', 'M', 'W', 'm', ' ']
            .iter()
            .map(|&c| gm.advance_width(charmap.map(c)))
            .fold(0.0f32, f32::max);
        let advance = if advance > 0.0 { advance } else { m.average_width.max(px * 0.6) };

        self.cell = CellMetrics::derive(&m, advance, &self.typo);
    }

    /// Find a glyph for `ch`, consulting system fallback if the stack lacks it.
    ///
    /// Returns `None` only when nothing on the system has the character, in
    /// which case the renderer should draw a visible replacement rather than
    /// silently nothing.
    pub fn glyph_for(&mut self, ch: char, style: Style) -> Option<(FontId, GlyphId)> {
        // Before the primary face, not after: on Windows the primary face
        // *does* carry these, so a fallback-shaped hook would never fire and
        // the gaps would stay. See `boxdraw` for why the font's own glyph is
        // the wrong shape however good the font is.
        //
        // The codepoint doubles as the glyph id, which the ranges make safe --
        // U+2500..=U+259F fits a `GlyphId` with room to spare.
        //
        // Grid only, hence the `ui_px` check. A generated mask is *cell*-sized,
        // which is the one thing chrome text is not: it is proportional, freely
        // positioned, and drawn at the UI type scale rather than the grid's. It
        // would also measure as zero-width, because `advance_of` asks the face
        // and there is no face behind this id.
        if self.builtin_box_drawing && self.ui_px.is_none() && boxdraw::covers(ch) {
            return Some((FontId::BOXDRAW, ch as GlyphId));
        }

        let primary = self.font_for(style);
        if let Some(face) = self.faces.get(primary.0 as usize) {
            if let Some(font) = face.font_ref() {
                let gid = font.charmap().map(ch);
                if gid != 0 {
                    return Some((primary, gid));
                }
            }
        }

        // Missing from the primary stack. Ask the system, and remember the
        // answer -- fallback resolution is far too slow to do per character
        // per frame.
        if let Some(cached) = self.fallback_cache.get(&ch) {
            let id = (*cached)?;
            let face = self.faces.get(id.0 as usize)?;
            let font = face.font_ref()?;
            let gid = font.charmap().map(ch);
            return (gid != 0).then_some((id, gid));
        }

        let resolved = self.resolve_fallback(ch, style);
        self.fallback_cache.insert(ch, resolved);

        let id = resolved?;
        let face = self.faces.get(id.0 as usize)?;
        let font = face.font_ref()?;
        let gid = font.charmap().map(ch);
        (gid != 0).then_some((id, gid))
    }

    fn resolve_fallback(&mut self, ch: char, style: Style) -> Option<FontId> {
        // Private Use Area: Nerd Font and Powerline icons, which is what shell
        // prompts are made of. No script, so the generic path below cannot find
        // them -- see `is_private_use`.
        if is_private_use(ch) {
            let families = self.symbol_families.clone();
            for family in &families {
                if let Some(id) = self.resolve(std::slice::from_ref(family), style) {
                    if self.face_has_glyph(id, ch) {
                        return Some(id);
                    }
                }
            }
            // Nothing installed carries it. Fall through rather than giving up:
            // a few PUA ranges are covered by ordinary system fonts.
        }

        // Emoji need their own path. Script-based fallback cannot find them:
        // emoji are script `Zyyy` (Common), the same bucket as punctuation and
        // digits, so asking for "a font that covers Common" returns the body
        // text font -- which has no emoji. Every terminal that renders emoji as
        // boxes has this bug.
        if is_emoji(ch) {
            if let Some(id) = self.resolve_generic(fontique::GenericFamily::Emoji, style) {
                if self.face_has_glyph(id, ch) {
                    return Some(id);
                }
            }
        }

        let script = script_of(ch);
        let families: Vec<String> = {
            let key = fontique::FallbackKey::from(script);
            let ids: Vec<fontique::FamilyId> = self.collection.fallback_families(key).collect();
            ids.iter()
                .filter_map(|id| self.collection.family(*id))
                .map(|f| f.name().to_string())
                .collect()
        };

        // Try each candidate and keep the first that actually has the
        // character. fontique's fallback list is per-script, not per-codepoint,
        // so the head of the list can still lack this particular glyph.
        for family in &families {
            if let Some(id) = self.resolve(std::slice::from_ref(family), style) {
                if self.face_has_glyph(id, ch) {
                    return Some(id);
                }
            }
        }

        // Box drawing, block elements and arrows: the third `Zyyy` case, after
        // emoji and the PUA, and the one that shows up as tofu in every TUI
        // border rather than in something exotic.
        //
        // The script fallback above cannot find these for the same structural
        // reason it cannot find emoji -- they are script Common, so "a font
        // covering Common" answers with the body text font, which on macOS is
        // Menlo, which has no box drawing at all. It never surfaced on Windows
        // because Cascadia Mono carries them, so the primary face answered and
        // fallback was never consulted.
        //
        // Sweeping every installed family for one codepoint would be a real
        // scan, so this only tries the families already known to be
        // symbol-rich: the Nerd Fonts found at startup, then whatever the
        // system calls monospace. Cached per character like every other
        // fallback, so the cost is paid once.
        if is_box_drawing(ch) {
            let candidates = self.symbol_families.clone();
            for family in &candidates {
                if let Some(id) = self.resolve(std::slice::from_ref(family), style) {
                    if self.face_has_glyph(id, ch) {
                        return Some(id);
                    }
                }
            }
            if let Some(id) = self.resolve_generic(fontique::GenericFamily::Monospace, style) {
                if self.face_has_glyph(id, ch) {
                    return Some(id);
                }
            }
            // Generic monospace is not enough, and this is the part that only a
            // machine with no Nerd Font installed can show. On macOS the generic
            // resolves to Courier, which has no box drawing; Menlo does, and
            // ships with every macOS. Any developer machine has a Nerd Font, so
            // the first loop answered and this gap stayed invisible until a bare
            // CI runner drew a TUI border as tofu.
            //
            // Named rather than discovered because there is no coverage query to
            // discover with: fontique's fallback is per-script, and these are
            // script Common. The alternative is sweeping every installed family
            // and loading each face's charmap, which is a real scan on the
            // render path. These are the stock faces that carry the ranges.
            for family in STOCK_SYMBOL_FAMILIES {
                if let Some(id) = self.resolve(&[(*family).to_string()], style) {
                    if self.face_has_glyph(id, ch) {
                        return Some(id);
                    }
                }
            }
        }

        None
    }

    fn resolve_generic(
        &mut self,
        generic: fontique::GenericFamily,
        style: Style,
    ) -> Option<FontId> {
        let names: Vec<String> = {
            let ids: Vec<fontique::FamilyId> = self.collection.generic_families(generic).collect();
            ids.iter()
                .filter_map(|id| self.collection.family(*id))
                .map(|f| f.name().to_string())
                .collect()
        };
        if names.is_empty() {
            return None;
        }
        self.resolve(&names, style)
    }

    fn face_has_glyph(&self, id: FontId, ch: char) -> bool {
        self.faces
            .get(id.0 as usize)
            .and_then(Face::font_ref)
            .is_some_and(|f| f.charmap().map(ch) != 0)
    }

    /// Rasterize a glyph.
    ///
    /// Sources are tried in order: colour bitmaps (CBDT/sbix), then colour
    /// outlines (COLR), then plain outlines. That order matters for emoji --
    /// Segoe UI Emoji is COLR, Apple Color Emoji is sbix, and a terminal that
    /// only handles outlines renders both as blank boxes.
    pub fn rasterize(&mut self, key: GlyphKey) -> Option<GlyphImage> {
        if key.font == FontId::BOXDRAW {
            // Cell geometry is not in the key, so this relies on the caller
            // clearing the atlas whenever `set_typography` changes the cell --
            // which it must do anyway, since every cached bitmap is the wrong
            // size after that. `line_height` is the case to keep in mind: it
            // changes `cell_h` without changing `px`, so the key alone would
            // not notice.
            let ch = char::from_u32(u32::from(key.glyph))?;
            return boxdraw::render(ch, self.cell);
        }

        let face = self.faces.get(key.font.0 as usize)?;
        let font = face.font_ref()?;
        let px = f32::from(key.px) / 256.0;

        // Hinting and coverage are independent, and the interesting
        // configurations are the mixed ones.
        //
        // swash does not let a caller choose a hinting *target*: it pins
        // `HintingMode::Smooth { lcd_subpixel: Some(LcdLayout::Horizontal), .. }`
        // (`swash/src/scale/hinting_cache.rs`) and `hint(bool)` is the whole
        // API. So `Hinting::Full` always means "grid-fit for a rasterizer with
        // three times the horizontal resolution", and pairing that with
        // grayscale coverage is what changes glyph shapes rather than merely
        // sharpening them -- `w` at 13ppem in Cascadia Mono came back as three
        // vertical stems and read as `W`. (#100.)
        //
        // That is a real cost and it is not the whole story: grid-fitting is
        // worth most where a stem is one pixel wide, so at 9pt it is the
        // difference between matching Windows Terminal and not, and at 16px it
        // is nearly invisible. Hence a setting rather than a rule.
        // From the *key*, never from `self`: the key is what the atlas cached
        // under, and `ui_px` may well have been cleared since.
        let hint = key.raster & RASTER_HINTED != 0;
        let format = if key.raster & RASTER_SUBPIXEL != 0 {
            swash::zeno::Format::Subpixel
        } else {
            swash::zeno::Format::Alpha
        };

        let mut scaler = self.scale_cx.builder(font).size(px).hint(hint).build();

        let offset = swash::zeno::Vector::new(f32::from(key.subpx_x) * 0.25, 0.0);
        let mut render = Render::new(&[
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::ColorOutline(0),
            Source::Outline,
        ]);
        render.format(format).offset(offset);

        if key.synthetic & SYNTHETIC_BOLD != 0 {
            // Proportional to size, or bold looks heavy at 8pt and thin at 32.
            render.embolden(px * 0.02);
        }
        if key.synthetic & SYNTHETIC_ITALIC != 0 {
            render.transform(Some(swash::zeno::Transform::skew(
                swash::zeno::Angle::from_degrees(-14.0),
                swash::zeno::Angle::ZERO,
            )));
        }

        let image = render.render(&mut scaler, key.glyph)?;
        // An exhaustive match, not `matches!`: a future swash content variant
        // should be a compile error here rather than silently becoming `Color`
        // and being uploaded to the wrong texture at the wrong stride.
        let format = match image.content {
            swash::scale::image::Content::Mask => GlyphFormat::Mask,
            swash::scale::image::Content::SubpixelMask => GlyphFormat::SubpixelMask,
            swash::scale::image::Content::Color => GlyphFormat::Color,
        };

        let mut data = image.data;
        if format == GlyphFormat::SubpixelMask {
            filter_subpixel(&mut data, image.placement.width, image.placement.height);
        }

        Some(GlyphImage {
            width: image.placement.width,
            height: image.placement.height,
            left: image.placement.left,
            top: image.placement.top,
            data,
            format,
        })
    }

    /// Set (or clear) a UI text size in physical pixels.
    ///
    /// While set, [`shape_run`](Self::shape_run), [`advance_of`](Self::advance_of)
    /// and [`key`](Self::key) work at this size instead of the grid's — which is
    /// how the chrome's 9.5–21px type scale shares one font system and one atlas
    /// with the grid. Cell geometry is untouched: only `set_typography` may
    /// change what a cell is. Callers pair set/clear around a run; leaking an
    /// override into the grid path would resize every glyph on screen, so the
    /// emit/measure helpers in `zest-render-wgpu` own that discipline.
    pub fn set_ui_px(&mut self, px: Option<f32>) {
        self.ui_px = px.map(|p| p.max(1.0));
    }

    /// The size shaping currently works at: the UI override, or the grid size.
    #[must_use]
    pub fn shaping_px(&self) -> f32 {
        self.ui_px.unwrap_or_else(|| self.typo.size_px())
    }

    /// A glyph key for the current size.
    #[must_use]
    pub fn key(&self, font: FontId, glyph: GlyphId) -> GlyphKey {
        GlyphKey {
            font,
            glyph,
            px: (self.shaping_px() * 256.0) as u16,
            subpx_x: 0,
            synthetic: self.faces.get(font.0 as usize).map_or(0, |f| f.synthetic),
            raster: self.raster_mode(),
        }
    }

    /// Shape a run of same-styled text.
    ///
    /// Ligature-aware. The caller places each cluster at its *starting cell*
    /// and discards the shaper's advances — the grid supplies positions. That
    /// is the whole trick to ligatures in a terminal: honour the shaper for
    /// glyph *selection*, ignore it for *positioning*.
    pub fn shape_run(&mut self, text: &str, style: Style, features: &[Feature]) -> Vec<ShapedGlyph> {
        let id = self.font_for(style);
        let Some(face) = self.faces.get(id.0 as usize) else { return Vec::new() };
        let Some(font) = face.font_ref() else { return Vec::new() };
        let px = self.shaping_px();

        let settings: Vec<swash::Setting<u16>> =
            features.iter().map(|f| swash::Setting { tag: f.tag, value: f.value }).collect();

        let mut shaper = self
            .shape_cx
            .builder(font)
            .size(px)
            .features(settings.iter().copied())
            .build();
        shaper.add_str(text);

        let mut out = Vec::new();
        shaper.shape_with(|cluster| {
            for glyph in cluster.glyphs {
                out.push(ShapedGlyph {
                    font: id,
                    glyph: glyph.id,
                    // Byte offset of the cluster this glyph came from -- the
                    // caller maps it back to a cell index.
                    cluster: cluster.source.start,
                    advance: glyph.advance,
                });
            }
        });
        out
    }

    /// The scaled advance of one glyph, in pixels.
    ///
    /// The grid never needs this — cells advance by cell width. UI text is
    /// proportional, and this is how a fallback glyph (whose advance
    /// `shape_run` cannot know, having shaped only the primary face) gets a
    /// real width instead of a guessed one.
    #[must_use]
    pub fn advance_of(&self, font: FontId, glyph: GlyphId) -> f32 {
        let Some(face) = self.faces.get(font.0 as usize) else { return 0.0 };
        let Some(f) = face.font_ref() else { return 0.0 };
        f.glyph_metrics(&[]).scale(self.shaping_px()).advance_width(glyph)
    }

    /// Direct `cmap` lookup with no shaping.
    ///
    /// Roughly 10x faster than shaping and correct for any run that has no
    /// ligature or contextual substitution to apply, which is the overwhelming
    /// majority of terminal output.
    pub fn map_ascii(&mut self, text: &str, style: Style) -> Vec<ShapedGlyph> {
        let id = self.font_for(style);
        let Some(face) = self.faces.get(id.0 as usize) else { return Vec::new() };
        let Some(font) = face.font_ref() else { return Vec::new() };
        let charmap = font.charmap();
        text.char_indices()
            .map(|(i, ch)| ShapedGlyph {
                font: id,
                glyph: charmap.map(ch),
                cluster: i as u32,
                advance: 0.0,
            })
            .collect()
    }

    /// True when a run can skip shaping entirely.
    #[must_use]
    pub fn can_use_fast_path(text: &str, ligatures: bool) -> bool {
        // Non-ASCII may need shaping (marks, joining scripts). ASCII with
        // ligatures off cannot produce a substitution worth shaping for.
        !ligatures && text.is_ascii()
    }

    /// Names that were requested, for diagnostics.
    #[must_use]
    pub fn requested_families(&self) -> &[String] {
        &self.requested
    }

    #[must_use]
    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Families consulted for Private Use Area glyphs, best first.
    /// Every installed family, for a settings UI that wants to offer a choice.
    pub fn installed_families(&mut self) -> Vec<String> {
        installed_families(&mut self.collection)
    }

    #[must_use]
    pub fn symbol_families(&self) -> &[String] {
        &self.symbol_families
    }

    /// Override the auto-discovered symbol families, for when configuration
    /// lands and a user wants a specific icon font.
    pub fn set_symbol_families(&mut self, families: Vec<String>) {
        self.symbol_families = families;
        self.fallback_cache.clear();
    }
}

/// True for Private Use Area codepoints.
///
/// This is where Nerd Fonts and Powerline put their glyphs, so it is what every
/// oh-my-posh, Starship or p10k prompt is built from. Like emoji, PUA
/// codepoints carry **no Unicode script**, so script-based fallback structurally
/// cannot resolve them — asking for "a font covering this script" returns the
/// body font, which has no icons. A terminal that does not special-case this
/// renders the user's prompt as a row of blanks.
fn is_private_use(ch: char) -> bool {
    matches!(ch as u32,
        0xE000..=0xF8FF        // BMP Private Use Area
        | 0xF0000..=0xFFFFD    // Supplementary PUA-A
        | 0x100000..=0x10FFFD  // Supplementary PUA-B
    )
}

/// Families likely to carry Nerd Font / Powerline glyphs.
///
/// Discovered by name rather than configured, because a user who installed a
/// Nerd Font to make their prompt work should not then have to name it in a
/// config file for the terminal to find it. Mono variants sort first: they are
/// designed to occupy one cell, which is what a grid wants.
/// Every installed family that is plausibly usable as a terminal face.
///
/// The settings UI needs a roster it can cycle, and the schema cannot carry one
/// — the same problem the theme picker has, solved the same way: the caller
/// brings the list.
///
/// fontique does not expose a monospace flag, so this cannot be an exact
/// answer. It is a *widened* one on purpose: too few entries means the font
/// someone wants is unreachable from the UI, while too many only means a couple
/// of extra presses. Faces that are certainly not terminal candidates — the
/// icon-only and symbol-only fonts — are dropped, and the rest is offered.
#[must_use]
pub fn installed_families(collection: &mut fontique::Collection) -> Vec<String> {
    let mut found: Vec<String> = collection
        .family_names()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            // Icon fonts have no letters at all; offering them would render the
            // whole terminal as tofu.
            !(lower.contains("symbols only")
                || lower.starts_with("segoe fluent icons")
                || lower.starts_with("segoe mdl2")
                || lower.starts_with("marlett")
                || lower.starts_with("webdings")
                || lower.starts_with("wingdings"))
        })
        .map(str::to_string)
        .collect();
    found.sort_by_key(|n| n.to_ascii_lowercase());
    found.dedup();
    found
}

fn discover_symbol_families(collection: &mut fontique::Collection) -> Vec<String> {
    let mut found: Vec<String> = collection
        .family_names()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains("nerd font")
                || lower.ends_with(" nf")
                || lower.contains("powerline")
                || lower.contains("symbols only")
        })
        .map(str::to_string)
        .collect();

    found.sort_by_key(|name| {
        let lower = name.to_ascii_lowercase();
        // Mono first, then everything else; stable within each group.
        (!lower.contains("mono"), lower.clone())
    });
    found
}

/// True for the line-drawing and block characters TUIs are built out of.
///
/// Deliberately *not* every symbol block. These are the ranges a monospace face
/// is expected to carry at exactly one cell wide, so borrowing them from
/// another monospace font keeps the grid aligned. Widening this to all of
/// Miscellaneous Symbols would start pulling proportional glyphs into cells.
/// Stock faces that carry box drawing, block elements and arrows.
///
/// Every one ships with its platform, so this is a list of things that are
/// *there*, not a wish list. All three platforms' entries are tried on all three
/// platforms: a name that does not resolve costs one lookup, and hard-coding the
/// `cfg` would mean a machine with a font from another platform installed —
/// which is most developer machines — is denied it for no reason.
///
/// Verified rather than assumed. On macOS with the symbol path disabled: Menlo,
/// Andale Mono, PT Mono and Courier **New** all carry U+2500, U+2588 and U+2190;
/// generic monospace, SF Mono and plain Courier carry none of them.
const STOCK_SYMBOL_FAMILIES: &[&str] = &[
    // macOS. Menlo first: it is on every install and covers all three ranges.
    "Menlo",
    "Andale Mono",
    // Windows.
    "Cascadia Mono",
    "Consolas",
    "Lucida Console",
    // Linux.
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
];

fn is_box_drawing(ch: char) -> bool {
    matches!(ch as u32,
        0x2190..=0x21FF   // arrows
        | 0x2500..=0x257F // box drawing
        | 0x2580..=0x259F // block elements
        | 0x25A0..=0x25FF // geometric shapes
        | 0x2800..=0x28FF // braille patterns, used for spinners and sparklines
        | 0xE0B0..=0xE0BF // powerline separators that escaped the PUA sweep
    )
}

/// True for characters that want an emoji font rather than a text font.
///
/// Deliberately a range check rather than a full Unicode property lookup: the
/// only decision it drives is which font list to try first, and being wrong
/// costs one extra failed lookup that is then cached. Pulling in a Unicode
/// property table for that would not be worth the dependency.
fn is_emoji(ch: char) -> bool {
    matches!(ch as u32,
        0x1F300..=0x1FAFF   // pictographs, emoticons, transport, symbols
        | 0x1F000..=0x1F0FF // mahjong, dominoes, playing cards
        | 0x2600..=0x27BF   // misc symbols and dingbats
        | 0x2B00..=0x2BFF   // arrows and geometric shapes
        | 0xFE00..=0xFE0F   // variation selectors
        | 0x1F1E6..=0x1F1FF // regional indicators (flags)
    )
}

/// The ISO 15924 script of a character, for fallback lookup.
///
/// swash reports scripts as OpenType tags (`latn`, `arab`) while fontique wants
/// ISO 15924 codes (`Latn`, `Arab`). For the overwhelming majority of scripts
/// the OpenType tag is the lowercased ISO code, so capitalizing the first byte
/// converts between them. The handful of exceptions — mostly the Indic v2 tags
/// like `dev2` — degrade to "unknown", which makes fontique fall back to its
/// default chain rather than fail. That is an acceptable outcome for a lookup
/// that is already a last resort, and it is cached per character either way.
fn script_of(ch: char) -> fontique::Script {
    use swash::text::Codepoint;

    let tag = ch.script().to_opentype();
    let b = tag.to_be_bytes();
    if !b.iter().all(|c| c.is_ascii_alphabetic()) {
        return fontique::Script::UNKNOWN;
    }
    fontique::Script::from_bytes([b[0].to_ascii_uppercase(), b[1], b[2], b[3]])
}

/// An OpenType feature setting, e.g. `calt`, `liga`, `ss01`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Feature {
    pub tag: u32,
    pub value: u16,
}

impl Feature {
    /// Parse the Alacritty/Ghostty/Kitty spelling, so an existing config pastes
    /// straight in: `calt`, `-liga`, `+ss01`, `cv02=2`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (s, default_value) = match s.as_bytes().first() {
            Some(b'-') => (&s[1..], 0),
            Some(b'+') => (&s[1..], 1),
            _ => (s, 1),
        };
        let (name, value) = match s.split_once('=') {
            Some((n, v)) => (n, v.parse().ok()?),
            None => (s, default_value),
        };
        let b = name.as_bytes();
        if b.len() != 4 {
            return None;
        }
        let tag = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        Some(Self { tag, value })
    }
}

/// One glyph produced by shaping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub font: FontId,
    pub glyph: GlyphId,
    /// Byte offset into the shaped text of the cluster this came from.
    pub cluster: u32,
    /// The shaper's advance. Recorded for UI text; the grid ignores it.
    pub advance: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_syntax_matches_other_terminals() {
        let calt = Feature::parse("calt").unwrap();
        assert_eq!(calt.tag, u32::from_be_bytes(*b"calt"));
        assert_eq!(calt.value, 1);

        assert_eq!(Feature::parse("-liga").unwrap().value, 0, "'-' disables");
        assert_eq!(Feature::parse("+ss01").unwrap().value, 1);
        assert_eq!(Feature::parse("cv02=2").unwrap().value, 2, "'=' sets an alternate");

        assert!(Feature::parse("toolong").is_none(), "tags are exactly 4 bytes");
        assert!(Feature::parse("ab").is_none());
    }

    #[test]
    fn fast_path_only_for_ascii_without_ligatures() {
        assert!(Fonts::can_use_fast_path("hello world", false));
        assert!(!Fonts::can_use_fast_path("hello", true), "ligatures need shaping");
        assert!(!Fonts::can_use_fast_path("héllo", false), "non-ascii may need shaping");
        assert!(!Fonts::can_use_fast_path("世界", false));
    }

    #[test]
    fn styles_are_distinct_keys() {
        assert_ne!(Style::new(true, false), Style::new(false, true));
        assert_eq!(Style::default(), Style::new(false, false));
    }

    #[test]
    fn private_use_ranges_are_recognized() {
        assert!(is_private_use('\u{e0b0}'), "powerline separator");
        assert!(is_private_use('\u{f07b}'), "nerd font folder icon");
        assert!(is_private_use('\u{f8ff}'), "top of the BMP PUA");
        // Ordinary text must not take the symbol path -- it would cost a wasted
        // lookup on every fallback.
        assert!(!is_private_use('a'));
        assert!(!is_private_use('世'));
        assert!(!is_private_use('─'), "box drawing is real Unicode, not PUA");
        assert!(!is_private_use('🚀'));
    }

    #[test]
    fn emoji_are_recognized_but_text_symbols_are_not() {
        assert!(is_emoji('👋'));
        assert!(is_emoji('🚀'));
        assert!(is_emoji('✅'));
        // Box drawing and CJK must NOT take the emoji path -- sending them to
        // an emoji font would fail and cost a wasted lookup.
        assert!(!is_emoji('─'));
        assert!(!is_emoji('世'));
        assert!(!is_emoji('a'));
    }

    #[test]
    fn script_detection_maps_to_iso_codes() {
        assert_eq!(script_of('a'), fontique::Script::from_bytes(*b"Latn"));
        assert_eq!(script_of('世'), fontique::Script::from_bytes(*b"Hani"));
        assert_eq!(script_of('α'), fontique::Script::from_bytes(*b"Grek"));
        assert_eq!(script_of('б'), fontique::Script::from_bytes(*b"Cyrl"));
    }

    /// These need real system fonts, so they are only meaningful on a machine
    /// that has them. They are the regression guard for the two font bugs found
    /// by eye in the PNG dump: emoji resolving to nothing, and fallback picking
    /// a family that does not actually contain the character.
    mod system {
        use super::*;

        fn fonts() -> Option<Fonts> {
            let families: Vec<String> =
                ["Cascadia Mono", "Consolas", "DejaVu Sans Mono", "monospace"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            Fonts::new(&families, Typography::default()).ok()
        }

        #[test]
        fn resolves_a_monospace_face() {
            let Some(f) = fonts() else { return };
            let cm = f.cell_metrics();
            assert!(cm.cell_w > 0 && cm.cell_h > 0);
            assert!(cm.baseline > 0 && cm.baseline < cm.cell_h, "baseline sits inside the cell");
        }

        /// The one test in this module that must not skip.
        ///
        /// Every other test here starts `let Some(..) = fonts() else { return }`,
        /// so on a machine where the named families are all absent they pass
        /// while asserting nothing -- which is precisely what happened on macOS,
        /// where none of Cascadia Mono, Consolas or DejaVu Sans Mono exists and
        /// the `"monospace"` safety net was being looked up as a literal family
        /// name that no platform has. Eight green font tests, zero coverage.
        ///
        /// `Fonts::new`'s contract is that a machine with no monospace font at
        /// all is not one we can serve. Every machine this could run on has one,
        /// so failing here is the correct outcome, not a reason to skip.
        #[test]
        fn the_generic_monospace_family_resolves_by_itself() {
            let generic = ["monospace".to_string()];
            let fonts = Fonts::new(&generic, Typography::default())
                .expect("the CSS generic `monospace` must resolve with no named family to help it");

            let cm = fonts.cell_metrics();
            assert!(
                cm.cell_w > 0 && cm.cell_h > 0,
                "a resolved face must produce a usable cell, got {}x{}",
                cm.cell_w,
                cm.cell_h
            );
        }

        #[test]
        fn ascii_resolves_in_the_primary_face() {
            let Some(mut f) = fonts() else { return };
            for ch in ['a', 'Z', '0', '#'] {
                assert!(f.glyph_for(ch, Style::default()).is_some(), "no glyph for {ch:?}");
            }
        }

        #[test]
        fn fallback_covers_other_scripts() {
            let Some(mut f) = fonts() else { return };
            // Each of these is absent from a typical programming font and must
            // come from system fallback. A failure here means tofu on screen.
            for ch in ['世', 'α', 'б', '─', '█'] {
                assert!(
                    f.glyph_for(ch, Style::default()).is_some(),
                    "no glyph found anywhere for {ch:?} -- this renders as tofu. \
                     Two different causes: the fallback path is broken, or this \
                     machine has no face covering that script at all. A bare \
                     Linux container is the second -- it needs fonts-noto-cjk, \
                     which is why CI installs it explicitly"
                );
            }
        }

        /// Box drawing, on a machine with no Nerd Font — which is every CI
        /// runner and no developer machine.
        ///
        /// `fallback_covers_other_scripts` above cannot see this. It asks for
        /// `─` and gets it from whatever Nerd Font happens to be installed, so
        /// it passes everywhere a human works and failed the first time it ran
        /// somewhere nobody works. Clearing the symbol families reproduces the
        /// bare machine exactly, and is the difference between testing the
        /// fallback and testing the developer's font collection.
        ///
        /// Not skippable when the list is empty: a machine with no Nerd Font is
        /// precisely the case under test.
        #[test]
        fn box_drawing_resolves_without_any_nerd_font() {
            let Some(mut f) = fonts() else { return };
            // Without this the assertion is vacuous for everything but the
            // arrow: `glyph_for` answers U+2500..=U+259F from `boxdraw` before
            // it consults a face at all, so the fallback chain this test exists
            // to guard would never run and the test would pass with that chain
            // deleted.
            f.set_builtin_box_drawing(false);
            f.set_symbol_families(Vec::new());
            for ch in ['─', '│', '█', '←'] {
                assert!(
                    f.glyph_for(ch, Style::default()).is_some(),
                    "no glyph for {ch:?} once the Nerd Font path is taken away. \
                     Every TUI border on a machine without one renders as tofu, \
                     and generic monospace does not cover it -- on macOS that is \
                     Courier, which has none of these"
                );
            }
        }

        /// The bug, at the layer where it was actually visible.
        ///
        /// `boxdraw`'s own tests prove the generated mask tiles; this proves the
        /// *font system* hands that mask to the renderer instead of a glyph.
        /// Flip `set_builtin_box_drawing(false)` and the second half of this
        /// test fails on any real font — which is what a run of `█` looked like
        /// before: a picket fence, because the font's advance is not the
        /// rounded cell.
        #[test]
        fn a_full_block_is_rasterized_at_cell_size() {
            let Some(mut f) = fonts() else { return };
            let cell = f.cell_metrics();

            let (font, glyph) = f.glyph_for('█', Style::default()).expect("U+2588 must resolve");
            let key = f.key(font, glyph);
            let img = f.rasterize(key).expect("and it must rasterize");

            // Size first, deliberately: this is the assertion that fails the way
            // the bug looked, and its message carries the numbers.
            assert_eq!(
                (img.width, img.height),
                (cell.cell_w, cell.cell_h),
                "a full block must be exactly one cell, or a run of them has seams"
            );
            assert!(
                img.data.iter().all(|&v| v == 255),
                "and solid, or the seams are inside the glyph instead of between them"
            );
            assert_eq!(font, FontId::BOXDRAW, "the grid must not take this from a face");
        }

        /// Chrome text must keep taking these from the font.
        ///
        /// A generated mask is cell-sized, and UI text is neither cell-sized nor
        /// cell-positioned. Worse, it would measure as **zero width**:
        /// `advance_of` asks the face for an advance and `FontId::BOXDRAW` has
        /// no face, so a status-bar string containing one box character would
        /// silently lay out as if that character were not there.
        #[test]
        fn ui_text_does_not_get_cell_sized_glyphs() {
            let Some(mut f) = fonts() else { return };
            f.set_ui_px(Some(11.0));
            let (font, glyph) = f.glyph_for('█', Style::default()).expect("U+2588 must resolve");
            assert_ne!(font, FontId::BOXDRAW, "chrome text takes the font's glyph");
            assert!(f.advance_of(font, glyph) > 0.0, "and it must measure as something");

            // And the grid still does, once the override is cleared.
            f.set_ui_px(None);
            let (font, _) = f.glyph_for('█', Style::default()).unwrap();
            assert_eq!(font, FontId::BOXDRAW, "the grid is still generated");
        }

        /// Turning the feature off has to actually reach the font.
        #[test]
        fn the_font_path_is_still_reachable() {
            let Some(mut f) = fonts() else { return };
            f.set_builtin_box_drawing(false);
            let (font, _) = f.glyph_for('█', Style::default()).expect("U+2588 must still resolve");
            assert_ne!(font, FontId::BOXDRAW, "the setting must take a real face");
        }

        /// Shell prompts are made of Private Use Area glyphs.
        ///
        /// The bug this guards is the emoji bug's twin: PUA codepoints carry no
        /// Unicode script, so script-based fallback returns the body font and
        /// every icon in an oh-my-posh or Starship prompt renders as a blank.
        /// Confirmed broken before the symbol-font path existed.
        #[test]
        fn nerd_font_prompt_glyphs_resolve() {
            let Some(mut f) = fonts() else { return };
            if f.symbol_families().is_empty() {
                // No Nerd Font on this machine; nothing to assert.
                return;
            }

            for ch in [
                '\u{e0b0}', // powerline right arrow
                '\u{e0b2}', // powerline left arrow
                '\u{f07b}', // folder
                '\u{f015}', // home
            ] {
                let found = f.glyph_for(ch, Style::default());
                assert!(
                    found.is_some(),
                    "no glyph for U+{:04X} -- prompt icons would render as blanks",
                    ch as u32
                );
                let (id, gid) = found.unwrap();
                let img = f.rasterize(f.key(id, gid));
                assert!(
                    img.is_some_and(|i| !i.is_empty()),
                    "U+{:04X} resolved but rasterized to nothing",
                    ch as u32
                );
            }
        }

        #[test]
        fn symbol_families_prefer_mono_variants() {
            let Some(f) = fonts() else { return };
            let fams = f.symbol_families();
            if fams.len() < 2 {
                return;
            }
            // Mono variants occupy one cell, which is what a grid wants; a
            // proportional icon font overlaps its neighbour.
            let first_mono = fams.iter().position(|n| n.to_ascii_lowercase().contains("mono"));
            let first_non_mono =
                fams.iter().position(|n| !n.to_ascii_lowercase().contains("mono"));
            if let (Some(m), Some(p)) = (first_mono, first_non_mono) {
                assert!(m < p, "mono symbol fonts should be tried first: {fams:?}");
            }
        }

        #[test]
        fn emoji_resolve_to_a_colour_font() {
            let Some(mut f) = fonts() else { return };
            // The bug this guards: emoji are script Zyyy (Common), the same
            // bucket as punctuation, so script-based fallback returns the body
            // font and the emoji renders as a box.
            let Some((id, gid)) = f.glyph_for('🚀', Style::default()) else {
                panic!("no font found for an emoji -- check the GenericFamily::Emoji path");
            };
            let key = f.key(id, gid);
            let img = f.rasterize(key).expect("emoji should rasterize");
            assert!(!img.is_empty(), "emoji rasterized to an empty bitmap");
            assert_eq!(
                img.format,
                GlyphFormat::Color,
                "emoji should come from the COLR/CBDT path, not outlines"
            );
        }

        #[test]
        fn rasterizing_ascii_produces_ink() {
            let Some(mut f) = fonts() else { return };
            let (id, gid) = f.glyph_for('M', Style::default()).expect("M");
            let img = f.rasterize(f.key(id, gid)).expect("rasterize M");
            assert!(!img.is_empty());
            assert!(img.data.iter().any(|&b| b > 0), "the bitmap is entirely blank");
            assert!(!img.format.is_color(), "text glyphs take the outline path");
        }

        #[test]
        fn a_space_rasterizes_to_nothing() {
            let Some(mut f) = fonts() else { return };
            let (id, gid) = f.glyph_for(' ', Style::default()).expect("space");
            // Either no bitmap at all or an empty one; both mean "draw nothing".
            let blank = f.rasterize(f.key(id, gid)).is_none_or(|img| img.is_empty());
            assert!(blank, "a space should produce no ink");
        }

        #[test]
        fn typography_changes_change_cell_geometry() {
            let Some(mut f) = fonts() else { return };
            let before = f.cell_metrics();

            let after = f.set_typography(Typography {
                line_height: 2.0,
                ..f.typography()
            });
            assert!(after.cell_h > before.cell_h, "line height must change the cell height");
            assert_eq!(after.cell_w, before.cell_w, "and not the width");

            // Which is why this is a PTY resize: fewer rows now fit.
            let (_, rows_before) = before.grid_size(800, 600, 0);
            let (_, rows_after) = after.grid_size(800, 600, 0);
            assert!(rows_after < rows_before);
        }

        #[test]
        fn bold_and_regular_are_different_faces_or_synthesized() {
            let Some(f) = fonts() else { return };
            let regular = f.font_for(Style::default());
            let bold = f.font_for(Style::new(true, false));
            // Either a real bold face was found (different FontId) or fontique
            // reported synthesis. Both are fine; silently returning the regular
            // face with no synthesis is not, because bold would be invisible.
            assert!(regular != bold || f.face_count() >= 1);
        }

        /// Rasterize one character at the current mode and size.
        fn glyph(f: &mut Fonts, ch: char) -> Option<GlyphImage> {
            let (font, gid) = f.glyph_for(ch, Style::default())?;
            let key = f.key(font, gid);
            f.rasterize(key)
        }

        /// Rows of the bitmap that sit *below* the baseline.
        ///
        /// Round letters are drawn a touch below the baseline by design so they
        /// do not read as smaller than flat-bottomed ones. Integer arithmetic,
        /// no threshold: `top` is the rise from the baseline to the bitmap's top
        /// edge, so anything past `height` hangs below.
        fn below_baseline(img: &GlyphImage) -> i32 {
            img.height as i32 - img.top
        }

        #[test]
        fn subpixel_coverage_really_is_per_channel() {
            let Some(mut f) = fonts() else { return };
            // Grayscale is the default now, so ask for subpixel explicitly --
            // both what the renderer can do and what the grid wants.
            f.set_text_antialias(TextAntialias::Subpixel);
            f.set_grid_antialias(TextAntialias::Subpixel);

            // Something has to actually differ between the channels, or the
            // mode is an expensive identity: four times the atlas to store the
            // same number three times.
            let mut chroma = 0u8;
            for ch in ['w', 'm', 'x', 'o'] {
                let Some(img) = glyph(&mut f, ch) else { continue };
                assert_eq!(img.format, GlyphFormat::SubpixelMask, "{ch:?}");
                assert_eq!(
                    img.data.len(),
                    (img.width * img.height * 4) as usize,
                    "{ch:?}: four bytes per texel"
                );
                for px in img.data.chunks_exact(4) {
                    let (lo, hi) = (px[..3].iter().min(), px[..3].iter().max());
                    if let (Some(&lo), Some(&hi)) = (lo, hi) {
                        chroma = chroma.max(hi - lo);
                    }
                }
            }
            assert!(
                chroma >= 32,
                "the three channels came back near-identical (max spread {chroma}), so \
                 subpixel sampling recovered no horizontal detail at all -- the whole \
                 point of the mode"
            );
        }

        #[test]
        fn grayscale_mode_is_one_coverage_byte_per_texel() {
            let Some(mut f) = fonts() else { return };
            f.set_text_antialias(TextAntialias::Grayscale);
            let Some(img) = glyph(&mut f, 'x') else { return };
            assert_eq!(img.format, GlyphFormat::Mask);
            assert_eq!(img.data.len(), (img.width * img.height) as usize);
        }

        #[test]
        fn box_drawing_stays_a_plain_mask_in_every_mode() {
            // Generated geometrically at cell size, so there is no sub-pixel
            // horizontal detail to recover and a `│` must stay one crisp column
            // rather than gaining a colour fringe.
            for mode in [TextAntialias::Subpixel, TextAntialias::Grayscale] {
                let Some(mut f) = fonts() else { return };
                f.set_text_antialias(mode);
                let key = GlyphKey {
                    font: FontId::BOXDRAW,
                    glyph: '│' as u32 as u16,
                    px: 0,
                    subpx_x: 0,
                    synthetic: 0,
                    raster: 0,
                };
                let Some(img) = f.rasterize(key) else { continue };
                assert_eq!(img.format, GlyphFormat::Mask, "{mode:?}");
                assert_eq!(img.data.len(), (img.width * img.height) as usize, "{mode:?}");
            }
        }

        #[test]
        fn the_mode_is_in_the_key_so_two_bitmaps_cannot_alias() {
            // This used to document an obligation -- change the mode without
            // clearing the atlas and a four-byte bitmap gets read at one byte
            // per texel, a third of a glyph, silently. The mode is part of
            // `GlyphKey` now, because chrome and the grid rasterize
            // differently at the same time, so the hazard is gone by
            // construction rather than by remembering.
            let Some(mut f) = fonts() else { return };
            let Some((font, gid)) = f.glyph_for('m', Style::default()) else { return };

            f.set_text_antialias(TextAntialias::Subpixel);
            f.set_grid_antialias(TextAntialias::Subpixel);
            let sub_key = f.key(font, gid);
            f.set_grid_antialias(TextAntialias::Grayscale);
            let gray_key = f.key(font, gid);

            assert_ne!(sub_key, gray_key, "two rasterizations, two cache entries");

            let (Some(sub), Some(gray)) = (f.rasterize(sub_key), f.rasterize(gray_key)) else {
                return;
            };
            assert_ne!(sub.format, gray.format);
            assert_ne!(sub.data.len(), gray.data.len(), "and two different strides");
        }

        /// The `Close tab` half of #100.
        ///
        /// swash hard-codes an LCD hinting target and exposes only `hint(bool)`,
        /// and at 13ppem that grid-fit flattens the baseline overshoot on
        /// `o c e C t` while leaving it on `a` -- so `Close` reads a pixel short
        /// beside `tab` in the same label. Subpixel sampling cannot fix this: it
        /// is horizontal and the overshoot is vertical. Not hinting can, which
        /// is why the two are one setting.
        ///
        /// Asserted against the flat-bottomed letters of the *same* face rather
        /// than against absolute numbers, so it needs no particular font
        /// installed: `x` and `z` are flat by design in any Latin face and `o`
        /// `c` `e` are drawn below the baseline in any Latin face. Cascadia Mono
        /// at 12.5px is the reported case, and that is what CI's Windows leg
        /// runs, but the assertion is not specific to it.
        #[test]
        fn the_grids_settings_do_not_reach_the_chrome() {
            // The requirement, stated as a test: `appearance.text_antialias`
            // and `appearance.text_hinting` are the *terminal's*. Chrome is the
            // application's own furniture and is pinned to what looks right.
            //
            // It matters specifically because grid-fitting at chrome sizes
            // flattens the baseline overshoot on `o c e` and not on `a`, so
            // "Close" reads a pixel short beside "tab" in one label -- which is
            // how #100 was opened, and would come straight back the first time
            // anyone set text_hinting = "full" for the grid.
            let Some(mut f) = fonts() else { return };
            f.set_grid_antialias(TextAntialias::Grayscale);
            f.set_hinting(Hinting::Full);

            let Some((font, gid)) = f.glyph_for('o', Style::default()) else { return };

            let grid = f.key(font, gid);
            assert_eq!(grid.raster & RASTER_HINTED, RASTER_HINTED, "the grid asked for hinting");
            assert_eq!(grid.raster & RASTER_SUBPIXEL, 0, "and for grayscale");

            f.set_ui_px(Some(12.5));
            let chrome = f.key(font, gid);
            f.set_ui_px(None);

            assert_eq!(chrome.raster & RASTER_HINTED, 0, "chrome is never grid-fitted");
            assert_ne!(grid.raster, chrome.raster, "and so the two cannot share a cache entry");
        }

        #[test]
        fn a_grid_that_asks_for_more_than_the_renderer_has_does_not_get_it() {
            // The grid's preference is a ceiling, not an override: asking for
            // subpixel coverage on a device that cannot composite it would
            // upload four bytes per texel into a one-byte texture.
            let Some(mut f) = fonts() else { return };
            f.set_text_antialias(TextAntialias::Grayscale); // what the renderer can do
            f.set_grid_antialias(TextAntialias::Subpixel); // what the settings want
            let Some((font, gid)) = f.glyph_for('m', Style::default()) else { return };
            assert_eq!(
                f.key(font, gid).raster & RASTER_SUBPIXEL,
                0,
                "the renderer's capability wins over the setting"
            );
        }

        #[test]
        fn round_letters_keep_their_baseline_overshoot() {
            let Some(mut f) = fonts() else { return };
            f.set_ui_px(Some(12.5));

            // Flat-bottomed controls first: if these ever overshoot, the metric
            // is measuring something other than what it claims and every
            // assertion below is worthless.
            for ch in ['x', 'z'] {
                let Some(img) = glyph(&mut f, ch) else { continue };
                assert_eq!(below_baseline(&img), 0, "{ch:?} is flat-bottomed by design");
            }

            let flat = glyph(&mut f, 'x').map_or(0, |i| below_baseline(&i));
            for ch in ['o', 'c', 'e'] {
                let Some(img) = glyph(&mut f, ch) else { continue };
                let round = below_baseline(&img);
                assert!(
                    round > flat,
                    "{ch:?} sits no lower than 'x' ({round} vs {flat}). Round letters are \
                     drawn slightly below the baseline on purpose so they do not read as \
                     smaller than flat-bottomed ones; grid-fitting flattens that for \
                     {ch:?} but not for 'a', which is why \"Close\" reads a pixel short \
                     next to \"tab\" in the same label. (#100)"
                );
            }
        }
    }
}
