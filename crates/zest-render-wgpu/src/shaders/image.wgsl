// The background-picture pipeline: one textured quad per viewport.
//
// This does not draw *over* the window background -- it **is** the window
// background. The offscreen is already cleared to `Scene::backdrop`, which
// carries `window.opacity` premultiplied, so a translucent quad blended on top
// of it would composite to `1-(1-o)^2` and the grid would come out visibly less
// transparent than the padding around it. That is the same trap
// `Scene::push_window_background` documents and skips a rect to avoid, so this
// pipeline is built with `blend: None` and emits the finished pixel instead.
//
// The consequence worth stating: at `dim = 1` the output must be `base`
// verbatim, byte for byte, because that is exactly what the rect this replaced
// would have written. `dimming_all_the_way_is_the_plain_background` pins it.

struct ImageInstance {
    // Destination rect in physical pixels -- the viewport.
    @location(0) rect: vec4<f32>,
    @location(1) clip: vec4<f32>,
    // Where the picture lands, in pixels *relative to* `rect.xy`. Outside it
    // there is no picture, only `base`: that is a Fit letterbox and a
    // Watermark's margins, both of which must match the plain background.
    @location(2) src: vec4<f32>,
    // The window background this viewport would otherwise have been filled
    // with: linear, premultiplied (ADR-003).
    @location(3) base: vec4<f32>,
    // 0 shows the picture as it is, 1 hides it completely.
    @location(4) dim: f32,
};

struct ImageVsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel: vec2<f32>,
    @location(1) rect: vec4<f32>,
    @location(2) clip_rect: vec4<f32>,
    @location(3) src: vec4<f32>,
    @location(4) base: vec4<f32>,
    @location(5) dim: f32,
};

@group(1) @binding(0) var picture: texture_2d<f32>;
@group(1) @binding(1) var picture_sampler: sampler;

@vertex
fn vs_image(@builtin(vertex_index) vi: u32, inst: ImageInstance) -> ImageVsOut {
    let corner = unit_quad(vi);
    // No `globals.grid_origin`: the picture is the window's, not the grid's, so
    // it must hold still while the rows smooth-scroll over it. The rect
    // pipeline omits it for the same reason.
    let pixel = mix(inst.rect.xy, inst.rect.xy + inst.rect.zw, corner);

    var out: ImageVsOut;
    out.clip_position = pixel_to_clip(pixel);
    out.pixel = pixel;
    out.rect = inst.rect;
    out.clip_rect = inst.clip;
    out.src = inst.src;
    out.base = inst.base;
    out.dim = inst.dim;
    return out;
}

@fragment
fn fs_image(in: ImageVsOut) -> @location(0) vec4<f32> {
    if clipped_out(in.pixel, in.clip_rect) {
        discard;
    }

    // The texture is `Rgba8UnormSrgb`, so this sample is already linear -- a
    // photograph is colour, unlike the atlas's coverage masks, which are
    // deliberately *not* sRGB views (ADR-010).
    let uv = (in.pixel - in.rect.xy - in.src.xy) / max(in.src.zw, vec2<f32>(1.0));
    let texel = textureSample(picture, picture_sampler, uv);

    // Outside the placement there is no picture at all. Computed as a weight
    // rather than a branch so the sample stays uniform-control-flow.
    let inside = f32(
        uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0
    );

    // `base` is premultiplied, so scaling the straight-alpha texel by `base.a`
    // puts both in the same space; mixing there is the same as mixing
    // un-premultiplied and multiplying afterwards, and it never divides by an
    // alpha that may be zero.
    let cover = texel.a * (1.0 - in.dim) * inside;
    let rgb = mix(in.base.rgb, texel.rgb * in.base.a, cover);
    return vec4<f32>(rgb, in.base.a);
}
