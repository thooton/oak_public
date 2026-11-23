// Liquid Glass's material: a port of liquid_glass_widgets' premium shader
// (shaders/liquid_glass_render.frag, MIT: Tim Lehmann for whynotmake.it and
// Sebastian Degenaar for pixel-innovations.com), its iOS 27 path: the
// paraxial lens, the frost (a cloud with a ghost of the content through
// it), the body's tint, saturation and shade, the meniscus, the rim light
// and the hairline, and the touch glint. Its geometry pass (an SDF matte
// with the surface normals) is computed here per pixel, for one rectangle
// with its corners (square ones too).
//
// Coordinates are the framebuffer's pixels, in the window's pixels (the
// scene glass is first drawn into has the window's size). `sharp` holds the
// backdrop (what was drawn before the glass) over `region`; `cloud` its
// weighted blur at a quarter of the size, over the same region.

struct Inst {
    rect: vec4f,    // x0, y0, x1, y1 (px)
    region: vec4f,  // the backdrop's origin and its texture's size (px)
    radii: vec4f,   // corner radii: top left, top right, bottom right, bottom left (px)
    glass: vec4f,   // glassColor, straight
    optics: vec4f,  // refractiveIndex, thickness (px), saturation, -
    light: vec4f,   // lightDirection, dprScale (px per point / 3), opacity
    rim: vec4f,     // rimShade, rimLight, rimShadeEnds, edgeAbsorption
    frost: vec4f,   // frostOpacity (0: clear), frostClamp, ghost sigma (px), blurWeight
    misc: vec4f,    // bodyShade, -, -, edge smoothing (px)
    touch: vec4f,   // touch position (px), touch intensity, -
    extra: vec4f,   // backdrop height (px), -, lightIntensity, -
};

@group(0) @binding(0) var<uniform> u: Inst;
@group(1) @binding(0) var sharp: texture_2d<f32>;
@group(1) @binding(1) var cloud: texture_2d<f32>;
@group(1) @binding(2) var smp: sampler;

const LUMA = vec3f(0.2126, 0.7152, 0.0722);

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);
}

// smoothstep, defined for a falling edge too.
fn sstep(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp((x - a) / (b - a), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn luma(c: vec3f) -> f32 {
    return dot(c, LUMA);
}

fn saturation(c: vec3f, s: f32) -> vec3f {
    let l = luma(c);
    return clamp(mix(vec3f(l), c, s), vec3f(0.0), vec3f(1.0));
}

// The signed distance to a box of half size `half` at the origin, its
// corners rounded by `r4` (top left, top right, bottom right, bottom left).
fn sd_box(p: vec2f, half: vec2f, r4: vec4f) -> f32 {
    let top = select(r4.x, r4.y, p.x >= 0.0);
    let bottom = select(r4.w, r4.z, p.x >= 0.0);
    let r = select(bottom, top, p.y < 0.0);
    let q = abs(p) - half + vec2f(r);
    return length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn region_uv(p: vec2f) -> vec2f {
    return (p - u.region.xy) / u.region.zw;
}

// The backdrop at `p`, bilinear, straight.
fn sharp_at(p: vec2f) -> vec3f {
    let c = textureSampleLevel(sharp, smp, region_uv(p), 0.0);
    return select(c.rgb, c.rgb / c.a, c.a > 0.001);
}

// The cloud at `p`: its weight is in its alpha (frostWeight).
fn cloud_at(p: vec2f) -> vec3f {
    let c = textureSampleLevel(cloud, smp, region_uv(p), 0.0);
    return select(c.rgb, c.rgb / c.a, c.a > 0.0001);
}

// The ghost: a Gaussian of the backdrop around `p`, each texel weighted by
// its luminance (white blurWeight times black).
fn ghost_at(p: vec2f) -> vec3f {
    let sigma = max(u.frost.z, 0.3);
    let inv = 1.0 / (2.0 * sigma * sigma);
    let slope = u.frost.w - 1.0;
    let base = floor(p);
    let r = i32(clamp(ceil(sigma * 2.0), 1.0, 3.0));
    var acc = vec3f(0.0);
    var ws = 0.0;
    for (var j = -3; j <= 3; j++) {
        for (var i = -3; i <= 3; i++) {
            if (abs(i) <= r && abs(j) <= r) {
                let t = base + vec2f(f32(i), f32(j)) + 0.5;
                let d = t - p;
                let c = sharp_at(t);
                let w = exp(-dot(d, d) * inv) * (1.0 + slope * luma(c));
                acc += w * c;
                ws += w;
            }
        }
    }
    return acc / max(ws, 1e-5);
}

fn highlight(bg: vec3f) -> vec3f {
    let l = luma(bg);
    let m = max(max(bg.r, bg.g), bg.b);
    let lf = l * 2.5 / (1.0 + l * 2.5);
    let sf = m * 2.5 / (1.0 + m * 2.5);
    return mix(vec3f(1.0), bg / max(l, 0.001), lf * sf);
}

// The body's tint: achromatic glass lifts toward its color, chromatic glass
// shifts the hue and keeps the backdrop's brightness.
fn apply_glass(c: vec3f, g: vec4f) -> vec3f {
    if (g.a <= 0.001) {
        return c;
    }
    let direct = mix(c, g.rgb, g.a);
    let tinted = clamp(g.rgb + (luma(c) - luma(g.rgb)), vec3f(0.0), vec3f(1.0));
    let chroma = max(max(g.r, g.g), g.b) - min(min(g.r, g.g), g.b);
    return mix(direct, mix(c, tinted, g.a), clamp(chroma * 8.0, 0.0, 1.0));
}

// The glass at pixel `frag`, premultiplied.
fn shade(frag: vec2f) -> vec4f {
    let ds = max(0.1, u.light.z);
    let lo = u.rect.xy;
    let size = u.rect.zw - u.rect.xy;
    let half = size * 0.5;
    let c = lo + half;
    let sd = sd_box(frag - c, half, u.radii);
    let st = ds;
    let dx = (sd_box(frag + vec2f(st, 0.0) - c, half, u.radii) - sd_box(frag - vec2f(st, 0.0) - c, half, u.radii)) / (2.0 * st);
    let dy = (sd_box(frag + vec2f(0.0, st) - c, half, u.radii) - sd_box(frag - vec2f(0.0, st) - c, half, u.radii)) / (2.0 * st);
    let gm = sqrt(dx * dx + dy * dy);
    let sdn = select(sd, sd / gm, gm > 0.1);
    let grad = select(vec2f(0.0), vec2f(dx, dy) / gm, gm > 1e-4);
    let alpha = sstep(u.misc.w * 0.5, -u.misc.w * 0.5, sdn) * u.light.w;
    if (alpha < 0.002) {
        return vec4f(0.0);
    }

    // The bevel: a circular arc `thickness` wide along the edge.
    let thick = max(u.optics.y, 0.001);
    var nxy = grad;
    var rim_dist = 0.0;
    if (sdn < 0.0) {
        let n_cos = max(thick + sdn, 0.0) / thick;
        let n_sin = sqrt(max(0.0, 1.0 - n_cos * n_cos));
        nxy = normalize(vec3f(grad * n_cos, n_sin)).xy;
        rim_dist = clamp(-sdn, 0.0, thick);
    }
    let rp = rim_dist / thick;
    let nh = sqrt(max(0.0, rp * (2.0 - rp)));
    let nl2 = dot(nxy, nxy);
    let nl = sqrt(nl2);
    let rim_n = nxy / max(nl, 1e-4);

    // Paraxial lens: the thin-prism law, (n - 1) times the slope.
    var disp = vec2f(0.0);
    if (nl2 >= 1e-4) {
        disp = -nxy * (u.optics.x - 1.0) * nl * thick * 8.0;
    }
    let reach = 0.5 * min(size.x, size.y);
    let r2 = dot(disp, disp);
    if (r2 > reach * reach) {
        disp *= reach / sqrt(r2);
    }
    let px_pt = 3.0 * ds;
    let rim_pt = rim_dist / px_pt;
    let lens_disp = disp;
    let frosted = u.frost.x > 0.0;
    let clear = !frosted;

    // The hairline: the backdrop just outside the shape, darkened last.
    var hair = 0.0;
    if (u.rim.x > 0.001 || (clear && u.rim.y > 0.001)) {
        hair = 1.0 - sstep(0.75 * ds, 2.0 * ds, rim_dist);
        hair *= 1.0 - step(0.999, nh);
        disp = mix(disp, rim_n * (2.5 * ds), hair);
    }

    var refr = vec3f(0.0);
    if (!frosted || hair > 0.0) {
        if (nl2 < 1e-4 && dot(disp, disp) < 1e-4) {
            refr = sharp_at(frag);
        } else {
            let rf = frag + disp;
            refr = sharp_at(rf);
            if (hair > 0.0) {
                let tang = vec2f(-nxy.y, nxy.x) / max(nl, 1e-4) * (1.5 * ds);
                let side = sharp_at(rf + tang) + sharp_at(rf - tang);
                refr = mix(refr, (refr + side) / 3.0, hair);
            }
        }
    }
    if (frosted && hair < 0.999) {
        let p = frag + lens_disp;
        let inward = rim_n * max(0.0, 4.0 * ds - rim_dist);
        let cl = cloud_at(frag - inward);
        var held = cl;
        if (u.frost.x < 1.0) {
            let gh = ghost_at(p);
            let k = u.frost.y;
            held = gh;
            if (k > 0.0) {
                held = max(gh, cl - vec3f(k));
            } else if (k < 0.0) {
                held = min(gh, cl - vec3f(k));
            }
        }
        let fr = mix(held, cl, clamp(u.frost.x, 0.0, 1.0));
        refr = mix(fr, refr, hair);
    }

    var g = u.glass;
    g.a *= 1.0 - hair;
    var col = apply_glass(refr, g);
    let bl = luma(refr);
    let adapt = mix(1.2, 0.8, bl);
    col = saturation(col, mix(u.optics.z * adapt, 1.0, hair));

    // The clear lens's diffuse lift on compact lenses, and its key shade.
    if (clear) {
        let gy = (frag.y - lo.y) / size.y;
        let trough = sstep(0.04, 0.16, gy) * (1.0 - sstep(0.22, 0.52, gy)) * sstep(1.5, 3.5, rim_pt);
        if (size.y < 48.0 * px_pt) {
            let lift = 0.105 * (1.0 - 0.45 * trough) * clamp(u.rim.y, 0.0, 1.0) * (1.0 - clamp(bl, 0.0, 1.0)) * (1.0 - hair);
            col = clamp(col + vec3f(lift), vec3f(0.0), vec3f(1.0));
        }
        let key = 0.025 * trough * clamp(bl, 0.0, 1.0) * (1.0 - hair);
        col = max(col - vec3f(key), vec3f(0.0));
    }
    col = mix(col, u.glass.rgb, u.glass.a * 0.12 * (adapt - 1.0) * (1.0 - hair));

    // Body shade: the dark material pulls a bright backdrop down.
    if (u.misc.x > 0.0) {
        let l = luma(col);
        col *= 1.0 - u.misc.x * l * l;
    }

    let light = u.light.xy;
    let litness = dot(rim_n, light);
    let bscale = 40.0 * ds;
    let tscale = clamp(bscale / max(thick, 1.0), 1.0, 4.0);
    let edge = 1.0 - sstep(0.0, mix(0.8, 0.5, 1.0 / tscale), nh);

    // Meniscus: the thicker rim absorbs more, less on the lit side.
    if (u.rim.w > 0.001 && nh < 0.999) {
        let rr = clamp(1.0 - nh, 0.0, 1.0);
        let rim_thick = 1.0 - sqrt(max(0.0, 1.0 - rr * rr));
        let dir_scale = mix(1.4, 0.6, litness * 0.5 + 0.5);
        col *= max(0.0, 1.0 - sqrt(rim_thick) * u.rim.w * dir_scale);
    }

    // The glint where it is pressed.
    let touch_light = max(u.extra.z, u.rim.y * 0.35);
    if (edge > 0.01 && u.touch.z > 0.001 && touch_light > 0.001) {
        let to = frag - u.touch.xy;
        let td = length(to);
        let dpr = max(1.0, ds * 3.0);
        let fall = sstep(max(70.0 * dpr, u.extra.x * 1.5), 0.0, td);
        if (fall > 0.001 && td > 1.0) {
            let rt = max(0.0, dot(rim_n, to / td));
            let rt2 = rt * rt;
            let raw = rt2 * rt2 * rt2 * fall * u.touch.z * touch_light * 2.5;
            col = mix(col, highlight(refr), raw / (1.0 + raw) * edge);
        }
    }

    // Rim light: two lobes at the ends of the light's axis.
    if (u.rim.y > 0.001 && rim_dist < 30.0 * ds) {
        let lit = max(litness, 0.0);
        let opp = max(-litness, 0.0);
        let lw = select(1.0, 0.84, clear);
        let ow = select(0.85, 1.02, clear);
        let lobe = lw * lit * lit * lit + ow * opp * opp * opp;
        let inset = max(0.0, rim_dist - 2.25 * ds) / ds;
        let core = exp(-inset / 1.5);
        let tail = exp(-inset / 8.0);
        let ease = pow(1.0 - clamp(luma(col), 0.0, 1.0), 0.3);
        let boost = select(1.0, 1.32, clear);
        let tw = select(0.035, 0.055, clear);
        let rl = u.rim.y * boost * lobe * ease * (0.175 * core + tw * tail) * (1.0 - hair);
        col = clamp(col + vec3f(rl), vec3f(0.0), vec3f(1.0));
    }

    // The hairline's darkening: strongest across the light's axis.
    if (hair > 0.0) {
        let a = max(1.0 - litness * litness, 0.0);
        let s = sqrt(a);
        let across = a * s * sqrt(s);
        let bright = sstep(0.35, 0.70, bl);
        var rs = u.rim.x;
        var ends = u.rim.z;
        if (clear) {
            rs = mix(u.rim.x, max(u.rim.x, u.rim.y * 1.15) * 1.12, bright);
            ends = mix(u.rim.z * 0.35, max(u.rim.z, 0.68), bright);
        }
        let dark = rs * 0.314 * mix(ends, 1.0, across);
        col = max(col - vec3f(dark * hair), vec3f(0.0));
    }

    return vec4f(col * alpha, alpha);
}

@fragment
fn fs_gamma(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    return shade(pos.xy);
}

// For an sRGB framebuffer, which encodes what it is given.
@fragment
fn fs_linear(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let c = shade(pos.xy);
    if (c.a <= 0.0) {
        return c;
    }
    let s = c.rgb / c.a;
    let lin = select(pow((s + 0.055) / 1.055, vec3f(2.4)), s / 12.92, s <= vec3f(0.04045));
    return vec4f(lin * c.a, c.a);
}
