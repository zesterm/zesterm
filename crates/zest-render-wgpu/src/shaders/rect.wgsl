// The SDF rect pipeline: everything rectangular.
//
// Cell backgrounds, selection, the cursor and its smear, and every piece of
// window chrome -- titlebar, tabs, buttons, panels, borders, palette, and later
// pane splitters. One pipeline rather than a plain-quad pipeline plus a chrome
// pipeline, which is why the renderer stays at three total.

struct RectInstance {
    @location(0) rect: vec4<f32>,
    @location(1) rect_b: vec4<f32>,
    @location(2) radii: vec4<f32>,
    @location(3) fill: vec4<f32>,
    @location(4) border: vec4<f32>,
    @location(5) clip: vec4<f32>,
    @location(6) border_width: f32,
    @location(7) shadow_blur: f32,
    @location(8) shadow_alpha: f32,
    @location(9) shape: u32,
};

struct RectVsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel: vec2<f32>,
    @location(1) rect: vec4<f32>,
    @location(2) rect_b: vec4<f32>,
    @location(3) radii: vec4<f32>,
    @location(4) fill: vec4<f32>,
    @location(5) border: vec4<f32>,
    @location(6) clip_rect: vec4<f32>,
    @location(7) params: vec3<f32>,   // border_width, shadow_blur, shadow_alpha
    @location(8) @interpolate(flat) shape: u32,
};

const SHAPE_ROUNDED_BOX: u32 = 0u;
const SHAPE_HULL_OF_TWO: u32 = 1u;

@vertex
fn vs_rect(@builtin(vertex_index) vi: u32, inst: RectInstance) -> RectVsOut {
    let corner = unit_quad(vi);

    // Expand for the shadow, or it gets clipped to the box that casts it.
    let pad = inst.shadow_blur * 2.0;

    // A hull spans both rects, so the quad has to cover their union.
    var lo = inst.rect.xy;
    var hi = inst.rect.xy + inst.rect.zw;
    if inst.shape == SHAPE_HULL_OF_TWO {
        lo = min(lo, inst.rect_b.xy);
        hi = max(hi, inst.rect_b.xy + inst.rect_b.zw);
    }
    lo -= vec2<f32>(pad);
    hi += vec2<f32>(pad);

    let pixel = mix(lo, hi, corner);

    var out: RectVsOut;
    out.clip_position = pixel_to_clip(pixel);
    out.pixel = pixel;
    out.rect = inst.rect;
    out.rect_b = inst.rect_b;
    out.radii = inst.radii;
    out.fill = inst.fill;
    out.border = inst.border;
    out.clip_rect = inst.clip;
    out.params = vec3<f32>(inst.border_width, inst.shadow_blur, inst.shadow_alpha);
    out.shape = inst.shape;
    return out;
}

// Signed distance to a box with per-corner radii. Negative inside.
fn sd_round_box(p: vec2<f32>, rect: vec4<f32>, radii: vec4<f32>) -> f32 {
    let half = rect.zw * 0.5;
    let centre = rect.xy + half;
    let q = p - centre;

    // Pick the radius for the quadrant this fragment is in.
    var r: f32;
    if q.x >= 0.0 {
        r = select(radii.y, radii.z, q.y >= 0.0);   // top-right, bottom-right
    } else {
        r = select(radii.x, radii.w, q.y >= 0.0);   // top-left, bottom-left
    }
    // A radius larger than half the box would invert the shape.
    r = min(r, min(half.x, half.y));

    let d = abs(q) - half + vec2<f32>(r);
    return length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0) - r;
}

@fragment
fn fs_rect(in: RectVsOut) -> @location(0) vec4<f32> {
    if clipped_out(in.pixel, in.clip_rect) {
        discard;
    }

    var d = sd_round_box(in.pixel, in.rect, in.radii);
    if in.shape == SHAPE_HULL_OF_TWO {
        // Union of the two boxes. A true convex hull would also fill the gap
        // between them; for a cursor smear the two boxes always overlap or abut,
        // so the union reads identically and costs one `min`.
        d = min(d, sd_round_box(in.pixel, in.rect_b, in.radii));
    }

    // fwidth gives one pixel of analytic antialiasing regardless of scale, so
    // this stays crisp at any DPI without supersampling.
    let aa = fwidth(d);
    let coverage = 1.0 - smoothstep(-aa, aa, d);

    var color = vec4<f32>(0.0);

    // Outer shadow first, so the box sits on top of it.
    let blur = in.params.y;
    let shadow_alpha = in.params.z;
    if blur > 0.0 && shadow_alpha > 0.0 {
        let s = 1.0 - smoothstep(-blur, blur, d);
        // Shadows are black; only their alpha varies. Premultiplied, so rgb is
        // zero and only the alpha channel carries the shadow.
        color = vec4<f32>(0.0, 0.0, 0.0, s * shadow_alpha);
    }

    // Border, then fill inside it.
    let bw = in.params.x;
    if bw > 0.0 {
        let inner = 1.0 - smoothstep(-aa, aa, d + bw);
        let ring = max(coverage - inner, 0.0);
        color = over(in.fill * inner, color);
        color = over(in.border * ring, color);
    } else {
        color = over(in.fill * coverage, color);
    }

    if color.a <= 0.0 {
        discard;
    }
    return color;
}

// Premultiplied source-over.
fn over(src: vec4<f32>, dst: vec4<f32>) -> vec4<f32> {
    return src + dst * (1.0 - src.a);
}
