// Liquid Glass's frost cloud: the backdrop at a quarter of its size, each
// texel weighted by its luminance (frostWeight: the weight goes in the
// alpha, the color is multiplied by it), then a separable Gaussian.

struct Blur {
    texel: vec2f,   // 1 / the source's size
    dir: vec2f,     // the blur's direction (0 for the first pass)
    sigma: f32,
    radius: f32,
    valid: vec2f,   // the source's texels that hold the backdrop
    base: f32,      // weight = base + slope * luma
    slope: f32,
    pad: vec2f,
};

@group(0) @binding(0) var<uniform> b: Blur;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);
}

// The source at `px` (its pixels), held inside what it holds.
fn tap(px: vec2f) -> vec4f {
    let c = clamp(px, vec2f(0.5), b.valid - vec2f(0.5));
    return textureSampleLevel(src, smp, c * b.texel, 0.0);
}

// A 4x4 box of the backdrop, from four bilinear taps, weighted.
@fragment
fn fs_down(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let s = pos.xy * 4.0;
    var acc = vec4f(0.0);
    for (var k = 0; k < 4; k++) {
        let o = vec2f(select(-1.0, 1.0, (k & 1) == 1), select(-1.0, 1.0, (k & 2) == 2));
        let c = tap(s + o);
        let rgb = select(c.rgb, c.rgb / c.a, c.a > 0.001);
        let w = b.base + b.slope * dot(rgb, vec3f(0.2126, 0.7152, 0.0722));
        acc += vec4f(rgb * w, w);
    }
    return acc * 0.25;
}

@fragment
fn fs_blur(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let r = i32(b.radius);
    let inv = 1.0 / (2.0 * b.sigma * b.sigma);
    var acc = vec4f(0.0);
    var ws = 0.0;
    for (var i = -r; i <= r; i++) {
        let f = f32(i);
        let w = exp(-f * f * inv);
        acc += tap(pos.xy + b.dir * f) * w;
        ws += w;
    }
    return acc / ws;
}
