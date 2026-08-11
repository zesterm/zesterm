// Shared by every pipeline.

struct Globals {
    target_size: vec2<f32>,
    grid_origin: vec2<f32>,
    text_gamma: f32,
    text_contrast: f32,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

// Pixel coordinates (origin top-left, y down) to clip space (origin centre,
// y up). Doing this in the shader means every instance format can stay in
// pixels, which is far easier to reason about and to test.
fn pixel_to_clip(p: vec2<f32>) -> vec4<f32> {
    let ndc = vec2<f32>(
        p.x / globals.target_size.x * 2.0 - 1.0,
        1.0 - p.y / globals.target_size.y * 2.0,
    );
    return vec4<f32>(ndc, 0.0, 1.0);
}

// The four corners of a unit quad, from a vertex index. No vertex buffer and no
// index buffer: `draw(0..4, 0..n)` as a triangle strip.
fn unit_quad(vertex_index: u32) -> vec2<f32> {
    return vec2<f32>(
        f32(vertex_index & 1u),
        f32((vertex_index >> 1u) & 1u),
    );
}

// Discard anything outside a clip rect.
//
// Done per-fragment rather than with set_scissor_rect so that clipping does not
// fragment a draw call -- the tab strip clips its overflow, and it should not
// cost a separate draw to do it.
fn clipped_out(p: vec2<f32>, clip: vec4<f32>) -> bool {
    return p.x < clip.x || p.y < clip.y
        || p.x > clip.x + clip.z || p.y > clip.y + clip.w;
}

// Coverage is a *perceptual* quantity, and the target it lands in is linear.
//
// A glyph's antialiased edge says "this pixel is 20% covered", meaning it
// should end up looking 20% of the way from the background to the text colour.
// But the offscreen is linear and the resolve encodes sRGB on the way out, so
// multiplying the colour by 0.2 there emerges at pow(0.2, 1/2.2) = 0.48 -- a
// fifth-covered pixel arrives half lit. Every edge is inflated, counters fill
// in, and the result reads exactly as "fatter and blurrier" beside a terminal
// that composites text in gamma space, which is what GDI and DirectWrite do.
//
// Measured against Windows Terminal on the same string: a stem's edge pixel sat
// at 83/255 where Windows Terminal had 0, and no pixel inside a `d` ever
// reached the background at all.
//
// So linearize the coverage before it is used as a weight. This is the sRGB
// transfer, not a tunable -- `text_gamma` is the taste knob and sits on top.
fn linearize_coverage(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((max(c, vec3<f32>(0.0)) + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}
