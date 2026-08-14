//! GPU instance formats.
//!
//! All colours here are **premultiplied and linear**. That is settled by ADR-003
//! and is not a per-pipeline choice: every pipeline blends with
//! `One / OneMinusSrcAlpha`, and mixing conventions is what produces dark halos
//! around text over a transparent window.

use bytemuck::{Pod, Zeroable};

/// A linear premultiplied colour, as the shaders consume it.
///
/// `Default` is [`Self::TRANSPARENT`] — the only sensible zero for a
/// premultiplied colour, and what `Zeroable` already produces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct LinearRgba(pub [f32; 4]);

impl LinearRgba {
    pub const TRANSPARENT: Self = Self([0.0, 0.0, 0.0, 0.0]);

    /// Convert an sRGB colour with a separate alpha into linear premultiplied.
    #[must_use]
    pub fn from_srgb(r: u8, g: u8, b: u8, alpha: f32) -> Self {
        let a = alpha.clamp(0.0, 1.0);
        let c = |v: u8| srgb_to_linear(f32::from(v) / 255.0) * a;
        Self([c(r), c(g), c(b), a])
    }

    #[must_use]
    pub fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self::from_srgb(r, g, b, 1.0)
    }

    #[must_use]
    pub fn is_transparent(&self) -> bool {
        self.0[3] <= 0.0
    }
}

/// The sRGB electro-optical transfer function.
///
/// Done on the CPU for instance colours because there are a few hundred of them
/// per frame at most, versus a few million fragments.
#[must_use]
pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Sides of a rect, for [`RectInstance::border_omit`].
///
/// CSS gets a one-sided stroke by giving the other three a width of zero, and
/// the *taper* falls out of that: a `border-left` on a box with `border-radius`
/// thins to nothing as the corner arc turns into the zero-width top border. The
/// block header's state rail and the active tab's accent rule are both that
/// shape, so the pipeline has to be able to say which sides a border skips —
/// clipping a full-thickness ring cuts it square instead, which is visibly the
/// wrong picture.
pub mod border_sides {
    pub const TOP: u32 = 1 << 0;
    pub const RIGHT: u32 = 1 << 1;
    pub const BOTTOM: u32 = 1 << 2;
    pub const LEFT: u32 = 1 << 3;
}

/// Shape selector for the rect pipeline.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RectShape {
    /// A rounded box. `radius = 0` gives a plain rectangle, which is the
    /// overwhelmingly common case (cell backgrounds and selection).
    RoundedBox = 0,
    /// The convex hull of two rounded boxes — the cursor smear. Costs one extra
    /// instance while the cursor is in flight and no extra pipeline.
    HullOfTwo = 1,
}

/// One instance for the SDF rect pipeline.
///
/// This single pipeline draws cell backgrounds, selection, the cursor, *and*
/// every piece of window chrome: the titlebar, tabs, buttons, panels, borders,
/// the command palette, and later pane splitters. It replaces the plain
/// background pipeline rather than adding to it, which is why the total stays at
/// three.
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct RectInstance {
    /// x, y, width, height in physical pixels.
    pub rect: [f32; 4],
    /// Second rect, used only by [`RectShape::HullOfTwo`].
    pub rect_b: [f32; 4],
    /// Per-corner radii: top-left, top-right, bottom-right, bottom-left.
    pub radii: [f32; 4],
    pub fill: LinearRgba,
    pub border: LinearRgba,
    /// Clip rectangle. Applied by discard rather than a scissor, so tab-strip
    /// overflow clipping does not fragment the draw call.
    pub clip: [f32; 4],
    pub border_width: f32,
    pub shadow_blur: f32,
    pub shadow_alpha: f32,
    pub shape: u32,
    /// Sides whose border is *not* drawn, from [`border_sides`].
    ///
    /// Phrased as an omission rather than a set of sides on purpose: zero is
    /// both what `Zeroable` produces and the honest reading "omit nothing", so
    /// every `..RectInstance::filled(..)` call site keeps its full ring without
    /// being touched. A set-of-sides field would default to "no border at all"
    /// and silently erase every existing stroke.
    pub border_omit: u32,
}

impl RectInstance {
    /// A plain filled rectangle — cell backgrounds and selection.
    #[must_use]
    pub fn filled(rect: [f32; 4], fill: LinearRgba, clip: [f32; 4]) -> Self {
        Self {
            rect,
            rect_b: [0.0; 4],
            radii: [0.0; 4],
            fill,
            border: LinearRgba::TRANSPARENT,
            clip,
            border_width: 0.0,
            shadow_blur: 0.0,
            shadow_alpha: 0.0,
            shape: RectShape::RoundedBox as u32,
            border_omit: 0,
        }
    }

    #[must_use]
    pub fn rounded(rect: [f32; 4], radius: f32, fill: LinearRgba, clip: [f32; 4]) -> Self {
        Self { radii: [radius; 4], ..Self::filled(rect, fill, clip) }
    }
}

/// Flags on a glyph instance.
pub mod glyph_flags {
    /// Sample the colour texture rather than the coverage mask.
    pub const COLOR: u32 = 1 << 0;
    /// Ignore `Globals::grid_origin`: this glyph belongs to the chrome and must
    /// hold still while the grid smooth-scrolls beneath it. The origin is a
    /// global uniform, not per-instance state, so without this bit tab titles
    /// would ride the scroll offset the moment `scroll_px` becomes nonzero.
    pub const FIXED: u32 = 1 << 1;
}

/// One instance for the glyph pipeline.
///
/// # Absolute pixels and RGBA, deliberately
///
/// Position is in **physical pixels** and colour is a full RGBA, rather than
/// `(row, col)` plus a palette index. That costs nothing now and is what lets
/// chrome labels, tab titles, the command palette, and M2 block headers reuse
/// this pipeline, the atlas, the shaper and the fallback machinery. Encoding
/// grid coordinates here would mean a second text renderer for the UI, and
/// retrofitting is a rewrite of every call site. → ADR-003 / ROADMAP sequencing.
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct GlyphInstance {
    /// Top-left of the glyph bitmap in physical pixels, bearing already applied.
    pub pos: [f32; 2],
    /// Texel coordinates within the atlas layer.
    pub uv: [f32; 2],
    /// Glyph bitmap size in pixels.
    pub size: [f32; 2],
    pub color: LinearRgba,
    pub clip: [f32; 4],
    pub layer: u32,
    pub flags: u32,
}

/// Decoration kinds, drawn after glyphs.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorKind {
    Underline = 0,
    DoubleUnderline = 1,
    /// Evaluated as a sine wave in the fragment shader — resolution independent
    /// and needs no atlas tile.
    Undercurl = 2,
    Strikethrough = 3,
}

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct DecorInstance {
    /// x, y, width, thickness in physical pixels.
    pub rect: [f32; 4],
    pub color: LinearRgba,
    pub clip: [f32; 4],
    pub kind: u32,
    pub _pad: [u32; 3],
}

/// Uniforms shared by every pipeline.
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct Globals {
    /// Target size in physical pixels, for the pixel-to-clip transform.
    pub target_size: [f32; 2],
    /// Sub-row scroll offset in pixels.
    ///
    /// Smooth scrolling translates grid geometry in the vertex shader and
    /// renders one extra row, rather than blitting or maintaining a scroll
    /// texture. Chrome instances pass 0.
    pub grid_origin: [f32; 2],
    /// Stem-darkening exponent applied at resolve. Light-on-dark text needs
    /// meaningfully more than dark-on-light, which is why it is a knob.
    pub text_gamma: f32,
    pub text_contrast: f32,
    pub _pad: [f32; 2],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instances_are_pod_and_tightly_packed() {
        // If any of these grow unexpectedly, per-frame upload cost grows with
        // them -- 30k glyph instances at 300x100 makes every byte count. Note
        // vertex attributes need no 16-byte padding; only uniforms do, which is
        // why GlyphInstance carries none.
        assert_eq!(std::mem::size_of::<GlyphInstance>(), 64);
        // 116, not 112: `border_omit` is worth its four bytes. Rect instances
        // are bounded by runs of same-coloured background, a few thousand a
        // frame, where the glyphs above are tens of thousands -- and the
        // alternative was packing the mask into `shape`'s spare bits, which
        // saves nothing measurable and hides the field from anyone reading the
        // struct.
        assert_eq!(std::mem::size_of::<RectInstance>(), 116);
        assert_eq!(std::mem::size_of::<DecorInstance>(), 64);
        // std140/430 alignment: uniform buffers need 16-byte alignment.
        assert_eq!(std::mem::size_of::<Globals>() % 16, 0);
    }

    #[test]
    fn srgb_conversion_hits_the_known_anchors() {
        assert!((srgb_to_linear(0.0) - 0.0).abs() < 1e-6);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        // Mid-grey sRGB 0.5 is about 0.214 linear. Getting this wrong is what
        // makes blended text look washed out or muddy.
        assert!((srgb_to_linear(0.5) - 0.2140).abs() < 1e-3);
    }

    #[test]
    fn colours_are_premultiplied() {
        // Half-transparent white: RGB must be scaled by alpha, not left at 1.0.
        // Leaving them unscaled is what produces bright halos where a
        // translucent background meets text.
        let c = LinearRgba::from_srgb(255, 255, 255, 0.5);
        assert!((c.0[0] - 0.5).abs() < 1e-5, "rgb must be premultiplied by alpha");
        assert!((c.0[3] - 0.5).abs() < 1e-5);

        let opaque = LinearRgba::opaque(255, 255, 255);
        assert!((opaque.0[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fully_transparent_colours_carry_no_ink() {
        let c = LinearRgba::from_srgb(255, 0, 0, 0.0);
        assert!(c.is_transparent());
        assert_eq!(c.0, [0.0, 0.0, 0.0, 0.0], "premultiplied by zero is zero");
    }

    #[test]
    fn a_plain_filled_rect_has_no_rounding_or_border() {
        let r = RectInstance::filled([0.0, 0.0, 10.0, 20.0], LinearRgba::opaque(255, 0, 0), [0.0, 0.0, 100.0, 100.0]);
        assert_eq!(r.radii, [0.0; 4]);
        assert_eq!(r.border_width, 0.0);
        assert_eq!(r.shape, RectShape::RoundedBox as u32);
        assert!(r.border.is_transparent());
    }

    #[test]
    fn a_rect_omits_no_side_unless_asked() {
        // Every bordered rect in the app is written as a struct update over
        // `filled`, so this default is what keeps their rings whole. Flip the
        // sense of the field and each one silently loses its border.
        let r = RectInstance::filled([0.0, 0.0, 10.0, 20.0], LinearRgba::opaque(255, 0, 0), [0.0, 0.0, 100.0, 100.0]);
        assert_eq!(r.border_omit, 0, "zero must mean a border on all four sides");
        assert_eq!(RectInstance::rounded([0.0, 0.0, 10.0, 20.0], 4.0, LinearRgba::TRANSPARENT, [0.0; 4]).border_omit, 0);
    }

    #[test]
    fn the_four_sides_are_distinct_bits() {
        use border_sides::{BOTTOM, LEFT, RIGHT, TOP};
        assert_eq!(TOP | RIGHT | BOTTOM | LEFT, 0b1111, "one bit each, no overlap");
    }
}
