// The glyph pipeline.
//
// Serves the terminal grid *and* all UI text from one atlas -- tab titles,
// chrome labels, the command palette, block headers. That is only possible
// because instances carry absolute pixel positions and RGBA rather than grid
// coordinates and a palette index.

struct GlyphInstance {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) color: vec4<f32>,
    @location(4) clip: vec4<f32>,
    @location(5) layer: u32,
    @location(6) flags: u32,
};

struct GlyphVsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) clip_rect: vec4<f32>,
    @location(4) @interpolate(flat) layer: u32,
    @location(5) @interpolate(flat) flags: u32,
};

const FLAG_COLOR: u32 = 1u;
// Kept in sync with instance::glyph_flags::FIXED by a test.
const FLAG_FIXED: u32 = 2u;

@group(1) @binding(0) var mask_atlas: texture_2d_array<f32>;
@group(1) @binding(1) var color_atlas: texture_2d_array<f32>;
@group(1) @binding(2) var atlas_sampler: sampler;

// One layer side, in texels. Kept in sync with atlas::LAYER_SIZE by a test.
const LAYER_SIZE: f32 = 2048.0;

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, inst: GlyphInstance) -> GlyphVsOut {
    let corner = unit_quad(vi);

    // grid_origin carries the sub-row smooth-scroll offset. It is a global
    // uniform, so chrome text opts out per instance with FLAG_FIXED — a tab
    // title must not move when the grid under it scrolls.
    let scrolled = (inst.flags & FLAG_FIXED) == 0u;
    let origin = select(vec2<f32>(0.0, 0.0), globals.grid_origin, scrolled);
    let pixel = inst.pos + corner * inst.size + origin;

    var out: GlyphVsOut;
    out.clip_position = pixel_to_clip(pixel);
    out.pixel = pixel;
    out.uv = (inst.uv + corner * inst.size) / LAYER_SIZE;
    out.color = inst.color;
    out.clip_rect = inst.clip;
    out.layer = inst.layer;
    out.flags = inst.flags;
    return out;
}

@fragment
fn fs_glyph(in: GlyphVsOut) -> @location(0) vec4<f32> {
    if clipped_out(in.pixel, in.clip_rect) {
        discard;
    }

    if (in.flags & FLAG_COLOR) != 0u {
        // Colour glyphs (COLR/CBDT/sbix -- emoji). The atlas is sRGB so the
        // sampler has already linearized RGB for us, and it stores
        // UNPREMULTIPLIED values: premultiplying before an sRGB encode does not
        // survive the round trip, because the encode is nonlinear. So multiply
        // through here instead.
        let texel = textureSample(color_atlas, atlas_sampler, in.uv, in.layer);
        let a = texel.a * in.color.a;
        if a <= 0.0 {
            discard;
        }
        return vec4<f32>(texel.rgb * a, a);
    }

    // Coverage mask. `in.color` is already premultiplied linear, so scaling the
    // whole vector by coverage keeps it premultiplied.
    var coverage = textureSample(mask_atlas, atlas_sampler, in.uv, in.layer).r;
    if coverage <= 0.0 {
        discard;
    }

    // Perceptual coverage -> linear weight; see `linearize_coverage`.
    coverage = linearize_coverage(vec3<f32>(coverage)).r;

    // Stem darkening, on the *coverage* and therefore only on text.
    //
    // Grayscale antialiasing systematically under-weights thin strokes, and the
    // effect is much stronger for light text on a dark background than the
    // reverse -- which is why this is a knob rather than a constant.
    //
    // This used to live in the resolve pass, applied to the whole framebuffer.
    // That is the same arithmetic pointed at the wrong thing: every pixel with
    // any alpha went through it, so cell backgrounds and chrome were lifted too
    // and a dark theme's background came out several shades off the colour the
    // theme asked for. Adjusting coverage is what "stem darkening" actually
    // means, and it leaves every solid fill in the frame untouched.
    //
    // Still a uniform read per fragment rather than baked into the atlas, so
    // tuning it stays a repaint and never a re-rasterization.
    if globals.text_gamma != 1.0 {
        coverage = pow(coverage, 1.0 / globals.text_gamma);
    }
    if globals.text_contrast != 0.0 {
        coverage = clamp((coverage - 0.5) * (1.0 + globals.text_contrast) + 0.5, 0.0, 1.0);
    }

    return in.color * coverage;
}
