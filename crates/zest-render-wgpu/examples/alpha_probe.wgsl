// Checkerboard with three alpha tiers, so per-pixel alpha can be judged by eye:
// opaque squares, half-transparent squares, and fully clear gaps.
//
// Output is PREMULTIPLIED and linear -- the same discipline the real pipelines
// use. The surface is sRGB, so the hardware does the encode.

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle. No vertex buffer, no index buffer.
    let x = f32(i32(i) / 2) * 4.0 - 1.0;
    let y = f32(i32(i) & 1) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let cell = vec2<i32>(pos.xy / 40.0);
    let parity = (cell.x + cell.y) & 1;
    let band = cell.x % 3;

    // Linear-space colors. A mid accent blue and a warm orange, chosen because
    // both show compositing errors clearly against typical desktops.
    var rgb: vec3<f32>;
    if (parity == 0) {
        rgb = vec3<f32>(0.11, 0.35, 0.85);
    } else {
        rgb = vec3<f32>(0.90, 0.45, 0.10);
    }

    var a: f32;
    if (band == 0) {
        a = 1.0;   // opaque -- must look solid
    } else if (band == 1) {
        a = 0.5;   // half -- desktop should show through, tinted
    } else {
        a = 0.0;   // clear -- desktop should show through untinted
    }

    // Premultiply. If the surface turns out to be PostMultiplied instead,
    // this line is the one that changes.
    return vec4<f32>(rgb * a, a);
}
