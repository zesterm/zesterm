// Offscreen -> surface.
//
// Everything is drawn into an Rgba16Float target and resolved here. One
// fullscreen triangle buys four things at once:
//
//  1. Blending happens in linear space, which is the only way compositing text
//     over a background is correct.
//  2. text_gamma / text_contrast are applied once, here, rather than baked into
//     every glyph bitmap. Tuning them becomes a free repaint instead of an
//     atlas rebuild.
//  3. Premultiplication in *encoded* space -- see below. This is the subtle one.
//  4. OS-driven repaints for free: when the compositor demands a redraw and
//     nothing is dirty, only this pass reruns from the retained offscreen. That
//     is what makes the 0%-GPU-at-idle claim survive window exposure events.

@group(0) @binding(0) var offscreen: texture_2d<f32>;
@group(0) @binding(1) var offscreen_sampler: sampler;

struct Params {
    text_gamma: f32,
    text_contrast: f32,
    _pad: vec2<f32>,
};
@group(0) @binding(2) var<uniform> params: Params;

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

    // Un-premultiply to get the true colour back before the transfer function.
    var rgb = src.rgb / max(a, 1e-5);

    // Stem darkening. Grayscale antialiasing systematically under-weights thin
    // strokes, and the effect is much stronger for light text on a dark
    // background than the reverse -- which is why this is a knob rather than a
    // constant. Without it, text looks anaemic on dark themes.
    if params.text_gamma != 1.0 {
        rgb = pow(rgb, vec3<f32>(1.0 / params.text_gamma));
    }
    if params.text_contrast != 0.0 {
        rgb = clamp((rgb - 0.5) * (1.0 + params.text_contrast) + 0.5, vec3<f32>(0.0), vec3<f32>(1.0));
    }

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
