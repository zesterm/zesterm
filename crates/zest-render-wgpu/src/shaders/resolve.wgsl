// Offscreen -> surface.
//
// Everything is drawn into an Rgba16Float target and resolved here. One
// fullscreen triangle buys four things at once:
//
//  1. Blending happens in linear space, which is the only way compositing text
//     over a background is correct.
//  2. Premultiplication in *encoded* space -- see below. This is the subtle one.
//  3. OS-driven repaints for free: when the compositor demands a redraw and
//     nothing is dirty, only this pass reruns from the retained offscreen. That
//     is what makes the 0%-GPU-at-idle claim survive window exposure events.
//
// text_gamma / text_contrast used to be applied here as well, which was the
// wrong place for them: this pass sees the finished frame, so the transfer
// function landed on cell backgrounds, selection and chrome as much as on text,
// and a dark theme's background arrived several shades lighter than the colour
// the theme asked for. They now adjust glyph *coverage* in `glyph.wgsl`, which
// is what stem darkening means and is still a per-fragment uniform read rather
// than something baked into the atlas -- so tuning stays a repaint.
//
// This pass is now purely a transfer: whatever was composited comes out
// unchanged apart from the sRGB encode. `a_solid_fill_survives_the_frame`
// pins that.

@group(0) @binding(0) var offscreen: texture_2d<f32>;
@group(0) @binding(1) var offscreen_sampler: sampler;

struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle, not a quad: no seam down the diagonal and one fewer
    // vertex. No buffers at all.
    let x = f32(i32(vi) / 2) * 4.0 - 1.0;
    let y = f32(i32(vi) & 1) * 4.0 - 1.0;

    var out: VsOut;
    out.clip_position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let src = textureSample(offscreen, offscreen_sampler, in.uv);
    let a = src.a;

    if a <= 0.0 {
        return vec4<f32>(0.0);
    }

    // Un-premultiply to get the true colour back before the encode.
    let rgb = src.rgb / max(a, 1e-5);

    // THE SUBTLE PART.
    //
    // DWM and Wayland want premultiplied *sRGB-encoded* values, but we blended
    // in linear. Encoding and then multiplying by alpha is not the same as
    // multiplying and then encoding, because the encode is nonlinear. Blending
    // straight into a Bgra8UnormSrgb surface with PreMultiplied alpha applies
    // the hardware encode *after* premultiplication and produces dark halos
    // around text over a transparent window.
    //
    // So: un-premultiply (above), encode, then re-premultiply here.
    let encoded = linear_to_srgb(rgb);
    return vec4<f32>(encoded * a, a);
}
