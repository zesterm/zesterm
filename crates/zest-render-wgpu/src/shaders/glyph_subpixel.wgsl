// The glyph fragment stage, with per-channel coverage.
//
// Appended to the grid module only in the dual-source variant, because naga
// validates `@blend_src` during *type* checking: a module that merely contains
// this struct fails `create_shader_module` on a device without
// `DUAL_SOURCE_BLENDING`, even if the entry point is never used. So the two
// variants are two modules, and only one is ever created.
//
// Everything else -- the instance layout, `vs_glyph`, the flags -- comes from
// glyph.wgsl, which is included in both variants. `fs_glyph` is still there and
// still the grayscale path; this is a second entry point beside it, never a
// replacement, which is what makes "grayscale is unchanged" true by
// construction rather than by review.

struct SubpixelOut {
    @location(0) @blend_src(0) color: vec4<f32>,
    @location(0) @blend_src(1) coverage: vec4<f32>,
};

@fragment
fn fs_glyph_subpixel(in: GlyphVsOut) -> SubpixelOut {
    if clipped_out(in.pixel, in.clip_rect) {
        discard;
    }

    var out: SubpixelOut;

    if (in.flags & FLAG_COLOR) != 0u {
        let texel = textureSample(color_atlas, atlas_sampler, in.uv, in.layer);
        let a = texel.a * in.color.a;
        if a <= 0.0 {
            discard;
        }
        out.color = vec4<f32>(texel.rgb * a, a);
        // The same scalar in all three lanes, so `OneMinusSrc1` degenerates to
        // `OneMinusSrcAlpha` exactly and emoji composite as they always have.
        out.coverage = vec4<f32>(a, a, a, a);
        return out;
    }

    // Three independent coverage samples of the same outline, taken a third of
    // a pixel apart. Byte 3 of the texel is never written by the rasterizer and
    // is deliberately not read.
    var cov = textureSample(mask_atlas, atlas_sampler, in.uv, in.layer).rgb;

    // `all`, not `any`: discarding a fragment because one channel is empty
    // punches a hole in exactly the leading or trailing subpixel column, which
    // is the one place the per-channel detail lives.
    if all(cov <= vec3<f32>(0.0)) {
        discard;
    }

    // Perceptual coverage -> linear weight, before anything else looks at it.
    // See `linearize_coverage` for what this costs when it is missing.
    cov = linearize_coverage(cov);

    // Stem darkening, componentwise -- which is the correct answer, not merely
    // the convenient one. Each channel is an independent sample at a different
    // x offset, and the transfer models the rasterizer under-weighting thin
    // strokes per sample. Collapsing the three to a scalar and scaling by it
    // would re-couple them and throw away the horizontal resolution this whole
    // path exists to recover. When the three are equal it reduces to exactly
    // the arithmetic in `fs_glyph`.
    if globals.text_gamma != 1.0 {
        cov = pow(cov, vec3<f32>(1.0 / globals.text_gamma));
    }
    if globals.text_contrast != 0.0 {
        cov = clamp(
            (cov - vec3<f32>(0.5)) * (1.0 + globals.text_contrast) + vec3<f32>(0.5),
            vec3<f32>(0.0),
            vec3<f32>(1.0),
        );
    }

    // Alpha *after* the transfer, or the colour and the alpha disagree and the
    // resolve pass's un-premultiply distorts the result.
    //
    // `max`, not a luminance mean: this is the scalar `resolve.wgsl` divides by,
    // and any value smaller than the strongest channel makes `rgb / a` exceed 1
    // on a fringed edge, which the sRGB encode then amplifies into a bloom.
    // `max` is the smallest scalar that cannot do that. Subpixel is gated to an
    // opaque destination, where the choice is unobservable anyway -- this is
    // about which way to be wrong if that gate ever moves.
    let a = max(cov.r, max(cov.g, cov.b)) * in.color.a;

    out.color = vec4<f32>(in.color.rgb * cov, a);
    out.coverage = vec4<f32>(in.color.a * cov, a);
    return out;
}
