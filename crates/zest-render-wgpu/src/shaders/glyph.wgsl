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

@group(1) @binding(0) var mask_atlas: texture_2d_array<f32>;
@group(1) @binding(1) var color_atlas: texture_2d_array<f32>;
@group(1) @binding(2) var atlas_sampler: sampler;

// One layer side, in texels. Kept in sync with atlas::LAYER_SIZE by a test.
const LAYER_SIZE: f32 = 2048.0;

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, inst: GlyphInstance) -> GlyphVsOut {
    let corner = unit_quad(vi);

    // grid_origin carries the sub-row smooth-scroll offset. Chrome passes zero,
    // so the same pipeline serves both without a branch or a second uniform.
    let pixel = inst.pos + corner * inst.size + globals.grid_origin;

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
    let coverage = textureSample(mask_atlas, atlas_sampler, in.uv, in.layer).r;
    if coverage <= 0.0 {
        discard;
    }
    return in.color * coverage;
}
