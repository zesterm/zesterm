// Underlines, strikethrough, and undercurl.
//
// Undercurl is a sine evaluated per-fragment rather than an atlas tile: it is
// resolution independent, costs no atlas space, and scales correctly with DPI.

struct DecorInstance {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) clip: vec4<f32>,
    @location(3) kind: u32,
};

struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel: vec2<f32>,
    @location(1) rect: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) clip_rect: vec4<f32>,
    @location(4) @interpolate(flat) kind: u32,
};

const KIND_UNDERLINE: u32 = 0u;
const KIND_DOUBLE_UNDERLINE: u32 = 1u;
const KIND_UNDERCURL: u32 = 2u;
const KIND_STRIKETHROUGH: u32 = 3u;

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: DecorInstance) -> VsOut {
    let corner = unit_quad(vi);

    // Undercurl and the double underline need vertical room beyond the nominal
    // thickness, or the wave is clipped flat.
    var extra = 0.0;
    if inst.kind == KIND_UNDERCURL {
        extra = inst.rect.w * 2.0;
    } else if inst.kind == KIND_DOUBLE_UNDERLINE {
        extra = inst.rect.w * 2.0;
    }

    let lo = inst.rect.xy - vec2<f32>(0.0, extra);
    let hi = inst.rect.xy + inst.rect.zw + vec2<f32>(0.0, extra);
    let pixel = mix(lo, hi, corner) + globals.grid_origin;

    var out: VsOut;
    out.clip_position = pixel_to_clip(pixel);
    out.pixel = pixel;
    out.rect = inst.rect;
    out.color = inst.color;
    out.clip_rect = inst.clip;
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    if clipped_out(in.pixel, in.clip_rect) {
        discard;
    }

    let thickness = max(in.rect.w, 1.0);
    let top = in.rect.y;
    let y = in.pixel.y;
    var coverage = 0.0;

    if in.kind == KIND_UNDERCURL {
        // One full wave per 6x thickness reads well from 8pt to 40pt.
        let period = max(thickness * 6.0, 4.0);
        let amplitude = thickness;
        // WGSL has no digit separators; this is 2*pi.
        let phase = (in.pixel.x - in.rect.x) / period * 6.2831855;
        let centre = top + thickness * 0.5 + sin(phase) * amplitude;
        let dist = abs(y - centre) - thickness * 0.5;
        coverage = 1.0 - smoothstep(-0.5, 0.5, dist);
    } else if in.kind == KIND_DOUBLE_UNDERLINE {
        // Two strokes with a gap of one thickness between them.
        let gap = thickness;
        let first = abs(y - (top + thickness * 0.5)) - thickness * 0.5;
        let second = abs(y - (top + thickness * 1.5 + gap)) - thickness * 0.5;
        coverage = 1.0 - smoothstep(-0.5, 0.5, min(first, second));
    } else {
        // Plain underline and strikethrough differ only in where the caller
        // placed the rect.
        let dist = abs(y - (top + thickness * 0.5)) - thickness * 0.5;
        coverage = 1.0 - smoothstep(-0.5, 0.5, dist);
    }

    // Horizontal extent, antialiased at the ends.
    let x0 = in.rect.x;
    let x1 = in.rect.x + in.rect.z;
    let hx = min(in.pixel.x - x0, x1 - in.pixel.x);
    coverage *= clamp(hx + 0.5, 0.0, 1.0);

    if coverage <= 0.0 {
        discard;
    }
    return in.color * coverage;
}
