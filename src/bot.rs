//! Oak's bots: the Oak Bots plugin's shapes (Grok Bot's drawings, the
//! plugin's `assets/bot-shapes.json`), as the Swarm view draws its members
//! (the graph's nodes, the running-agents list's icons). Each member gets
//! one shape by its name, drawn solid: the eyes the plugin cuts out are left
//! out here (`assets/bot-shapes.json` is the plugin's file without them), so
//! no bot has eyes. The shapes are rasterized from their paths for the size
//! they are drawn at (once, then kept as textures), so they are crisp from
//! the list's 14 points up, and tinted with any color.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use eframe::egui::{self, Color32, Painter, Pos2, Rect, TextureHandle, pos2, vec2};

/// The shapes, in the plugin's order (`botShapes`), which `shape_of` picks from.
pub(crate) const SHAPES: [&str; 26] = [
    "blob", "pebble", "bean", "egg", "squircle", "tablet", "capsule", "cylinder", "hex", "gem", "crystal", "wedge", "shield", "dome", "arch",
    "cloud", "teardrop", "leaf", "square", "sparkle", "clover", "flower", "heart", "house", "star", "pumpkin",
];

/// The biggest texture side drawn, in pixels (bigger bots are scaled up).
const MAX_PX: u32 = 256;

/// A shape's outline, in its own cell (0..1 both ways, y down): its body's
/// polygons and its extras (the pumpkin's stem), each with its color.
struct Shape {
    body: Vec<Vec<[f32; 2]>>,
    extras: Vec<(Vec<Vec<[f32; 2]>>, Color32)>,
}

/// The shape a member is drawn in, by its name: stable across runs, and
/// unrelated to its board hue (another hash).
pub(crate) fn shape_of(name: &str) -> usize {
    let h = name.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    h as usize % SHAPES.len()
}

/// Member `name`'s bot in `color`, centered on `center`, in a square
/// `size` points wide: its shape, solid, with its extras over it.
pub(crate) fn paint(p: &Painter, center: Pos2, size: f32, color: Color32, name: &str) {
    if size < 1.0 {
        return;
    }
    let ppp = p.ctx().pixels_per_point();
    let px = ((size * ppp).round() as u32).clamp(4, MAX_PX);
    // Animated sizes come in steps, so only a few textures are made.
    let px = if px > 48 { px.div_ceil(4) * 4 } else { px };
    let (body, extras) = textures(p.ctx(), shape_of(name), px);
    // On whole pixels, so a small bot stays sharp.
    let side = size.max(px as f32 / ppp);
    let min = ((center.to_vec2() - vec2(side, side) / 2.0) * ppp).round() / ppp;
    let rect = Rect::from_min_size(min.to_pos2(), vec2(side, side));
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    p.image(body.id(), rect, uv, color);
    if let Some(e) = extras {
        p.image(e.id(), rect, uv, Color32::WHITE.gamma_multiply(color.a() as f32 / 255.0));
    }
}

type Textures = (TextureHandle, Option<TextureHandle>);

thread_local! {
    /// Each shape drawn so far, by shape and size in pixels.
    static TEXTURES: RefCell<HashMap<(usize, u32), Textures>> = RefCell::new(HashMap::new());
}

/// Shape `i` at `px` pixels: its body in white (tinted when drawn) and its
/// extras in their own colors, if it has any.
fn textures(ctx: &egui::Context, i: usize, px: u32) -> Textures {
    if let Some(t) = TEXTURES.with(|t| t.borrow().get(&(i, px)).cloned()) {
        return t;
    }
    let s = &shapes()[i];
    let n = px as usize;
    let body = raster(&s.body, n);
    let rgba: Vec<u8> = body.iter().flat_map(|&a| [255, 255, 255, a]).collect();
    let opts = egui::TextureOptions::LINEAR;
    let body = ctx.load_texture(format!("bot-{i}-{px}"), egui::ColorImage::from_rgba_unmultiplied([n, n], &rgba), opts);
    let extras = (!s.extras.is_empty()).then(|| {
        let mut rgba = vec![0u8; n * n * 4];
        for (polys, c) in &s.extras {
            for (k, a) in raster(polys, n).into_iter().enumerate() {
                if a > 0 {
                    let px = &mut rgba[k * 4..k * 4 + 4];
                    let (fa, ba) = (a as f32 / 255.0, px[3] as f32 / 255.0);
                    let oa = fa + ba * (1.0 - fa);
                    for (ch, v) in [c.r(), c.g(), c.b()].into_iter().enumerate() {
                        px[ch] = ((v as f32 * fa + px[ch] as f32 * ba * (1.0 - fa)) / oa).round() as u8;
                    }
                    px[3] = (oa * 255.0).round() as u8;
                }
            }
        }
        ctx.load_texture(format!("bot-{i}-{px}-extras"), egui::ColorImage::from_rgba_unmultiplied([n, n], &rgba), opts)
    });
    let t = (body, extras);
    TEXTURES.with(|m| m.borrow_mut().insert((i, px), t.clone()));
    t
}

/// The shapes, parsed once from the embedded file.
fn shapes() -> &'static [Shape] {
    static SHAPES_PARSED: OnceLock<Vec<Shape>> = OnceLock::new();
    SHAPES_PARSED.get_or_init(|| {
        let all: serde_json::Value = serde_json::from_str(include_str!("../assets/bot-shapes.json")).expect("built-in bot shapes parse");
        SHAPES.iter().map(|name| parse_shape(&all[*name])).collect()
    })
}

/// A shape's entry (`vb`, `body`, `transform`?, `extras`?), mapped into its
/// unit cell.
fn parse_shape(v: &serde_json::Value) -> Shape {
    let nums = |s: &str| numbers(s);
    let vb = nums(v["vb"].as_str().unwrap_or("0 0 100 100"));
    let (vx, vy, vw, vh) = (vb[0], vb[1], vb[2].max(1e-3), vb[3].max(1e-3));
    let m = v["transform"]
        .as_str()
        .map(|t| nums(t.trim_start_matches("matrix")))
        .filter(|m| m.len() == 6)
        .unwrap_or_else(|| vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    let map = |polys: Vec<Vec<[f32; 2]>>| -> Vec<Vec<[f32; 2]>> {
        polys
            .into_iter()
            .map(|poly| {
                poly.into_iter()
                    .map(|[x, y]| {
                        let (tx, ty) = (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
                        [(tx - vx) / vw, (ty - vy) / vh]
                    })
                    .collect()
            })
            .collect()
    };
    let body = map(path(v["body"].as_str().unwrap_or("")));
    let extras = v["extras"]
        .as_array()
        .map(|es| {
            es.iter()
                .map(|e| {
                    let fill = e["fill"].as_str().unwrap_or("#000").trim_start_matches('#');
                    let c = u32::from_str_radix(fill, 16).unwrap_or(0);
                    (map(path(e["d"].as_str().unwrap_or(""))), Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8))
                })
                .collect()
        })
        .unwrap_or_default();
    Shape { body, extras }
}

/// The numbers in `s`, in order (signs, decimals, exponents).
fn numbers(s: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<f32>| {
        if let Ok(v) = cur.parse() {
            out.push(v);
        }
        cur.clear();
    };
    for ch in s.chars() {
        match ch {
            '0'..='9' => cur.push(ch),
            '.' if cur.contains('.') && !cur.contains(['e', 'E']) => {
                flush(&mut cur, &mut out);
                cur.push('.');
            }
            '.' | 'e' | 'E' => cur.push(ch),
            '-' | '+' if cur.ends_with(['e', 'E']) => cur.push(ch),
            '-' | '+' => {
                flush(&mut cur, &mut out);
                cur.push(ch);
            }
            _ => flush(&mut cur, &mut out),
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// An SVG path's closed polygons (absolute M, L, A, Z: what the shapes use;
/// arcs flattened).
fn path(d: &str) -> Vec<Vec<[f32; 2]>> {
    let mut polys: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut poly: Vec<[f32; 2]> = Vec::new();
    // Split into (command, its numbers).
    let mut parts: Vec<(char, String)> = Vec::new();
    for ch in d.chars() {
        if ch.is_ascii_alphabetic() && ch != 'e' && ch != 'E' {
            parts.push((ch, String::new()));
        } else if let Some(last) = parts.last_mut() {
            last.1.push(ch);
        }
    }
    let mut at = [0.0f32, 0.0];
    for (cmd, args) in parts {
        let n = numbers(&args);
        match cmd {
            'M' | 'L' => {
                for (k, xy) in n.chunks_exact(2).enumerate() {
                    if cmd == 'M' && k == 0 && poly.len() > 2 {
                        polys.push(std::mem::take(&mut poly));
                    } else if cmd == 'M' && k == 0 {
                        poly.clear();
                    }
                    at = [xy[0], xy[1]];
                    poly.push(at);
                }
            }
            'A' => {
                for a in n.chunks_exact(7) {
                    let to = [a[5], a[6]];
                    arc(&mut poly, at, a[0], a[1], a[2], a[3] != 0.0, a[4] != 0.0, to);
                    at = to;
                }
            }
            'Z' | 'z' => {
                if poly.len() > 2 {
                    polys.push(std::mem::take(&mut poly));
                }
                poly.clear();
            }
            _ => {}
        }
    }
    if poly.len() > 2 {
        polys.push(poly);
    }
    polys
}

/// An SVG elliptical arc from `from` to `to`, flattened onto `out` (SVG 1.1
/// F.6.5, endpoint to center).
#[allow(clippy::too_many_arguments)]
fn arc(out: &mut Vec<[f32; 2]>, from: [f32; 2], rx: f32, ry: f32, rot_deg: f32, large: bool, sweep: bool, to: [f32; 2]) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx < 1e-6 || ry < 1e-6 || from == to {
        out.push(to);
        return;
    }
    let (s, c) = rot_deg.to_radians().sin_cos();
    let (dx, dy) = ((from[0] - to[0]) / 2.0, (from[1] - to[1]) / 2.0);
    let (x1, y1) = (c * dx + s * dy, -s * dx + c * dy);
    let l = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry);
    if l > 1.0 {
        rx *= l.sqrt();
        ry *= l.sqrt();
    }
    let num = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let k = if den > 0.0 { (num / den).sqrt() } else { 0.0 } * if large == sweep { -1.0 } else { 1.0 };
    let (cx1, cy1) = (k * rx * y1 / ry, -k * ry * x1 / rx);
    let (cx, cy) = (c * cx1 - s * cy1 + (from[0] + to[0]) / 2.0, s * cx1 + c * cy1 + (from[1] + to[1]) / 2.0);
    let angle = |ux: f32, uy: f32| uy.atan2(ux);
    let t1 = angle((x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut dt = angle((-x1 - cx1) / rx, (-y1 - cy1) / ry) - t1;
    let tau = std::f32::consts::TAU;
    if sweep && dt < 0.0 {
        dt += tau;
    } else if !sweep && dt > 0.0 {
        dt -= tau;
    }
    let steps = ((dt.abs() / tau * 96.0).ceil() as usize).max(2);
    for i in 1..=steps {
        let t = t1 + dt * i as f32 / steps as f32;
        let (ex, ey) = (rx * t.cos(), ry * t.sin());
        out.push([c * ex - s * ey + cx, s * ex + c * ey + cy]);
    }
}

/// `polys` (in the unit cell, nonzero rule) filled into an `n` by `n`
/// alpha mask: exact coverage across each row, 6 samples down each pixel.
fn raster(polys: &[Vec<[f32; 2]>], n: usize) -> Vec<u8> {
    const SS: usize = 6;
    let w = 1.0 / SS as f32;
    let nf = n as f32;
    let edges: Vec<[f32; 4]> = polys
        .iter()
        .flat_map(|p| (0..p.len()).map(move |i| (p[i], p[(i + 1) % p.len()])))
        .map(|(a, b)| [a[0] * nf, a[1] * nf, b[0] * nf, b[1] * nf])
        .filter(|e| e[1] != e[3])
        .collect();
    let mut acc = vec![0f32; n * n];
    let mut xs: Vec<(f32, i32)> = Vec::new();
    for row in 0..n * SS {
        let y = (row as f32 + 0.5) * w;
        xs.clear();
        for e in &edges {
            if (e[1] <= y) != (e[3] <= y) {
                xs.push((e[0] + (y - e[1]) * (e[2] - e[0]) / (e[3] - e[1]), if e[3] > e[1] { 1 } else { -1 }));
            }
        }
        xs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let line = &mut acc[(row / SS) * n..(row / SS + 1) * n];
        let (mut wind, mut start) = (0, 0.0);
        for &(x, d) in &xs {
            let was = wind;
            wind += d;
            if was == 0 && wind != 0 {
                start = x;
            } else if was != 0 && wind == 0 {
                span(line, start, x, w);
            }
        }
    }
    acc.into_iter().map(|a| (a.min(1.0) * 255.0).round() as u8).collect()
}

/// Adds `w` of coverage to `line` from `a` to `b` (pixels, fractional ends).
fn span(line: &mut [f32], a: f32, b: f32, w: f32) {
    let n = line.len() as f32;
    let (a, b) = (a.clamp(0.0, n), b.clamp(0.0, n));
    if b <= a {
        return;
    }
    let (ia, ib) = (a.floor() as usize, b.floor() as usize);
    if ia == ib {
        line[ia.min(line.len() - 1)] += (b - a) * w;
        return;
    }
    line[ia] += (ia as f32 + 1.0 - a) * w;
    for v in &mut line[ia + 1..ib] {
        *v += w;
    }
    if ib < line.len() {
        line[ib] += (b - ib as f32) * w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_is_drawn_solid() {
        let n = 64;
        for (i, name) in SHAPES.iter().enumerate() {
            let a = raster(&shapes()[i].body, n);
            let filled = a.iter().filter(|&&v| v > 127).count();
            assert!(filled > n * n / 5, "{name} is drawn ({filled})");
            // No eyes: every clear pixel is reached from the border, so the
            // body has no holes.
            let mut seen = vec![false; n * n];
            let mut stack: Vec<usize> = (0..n).flat_map(|k| [k, n * (n - 1) + k, k * n, k * n + n - 1]).filter(|&k| a[k] < 128).collect();
            while let Some(k) = stack.pop() {
                if seen[k] || a[k] >= 128 {
                    continue;
                }
                seen[k] = true;
                let (x, y) = (k % n, k / n);
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if (0..n as i32).contains(&nx) && (0..n as i32).contains(&ny) {
                        stack.push(ny as usize * n + nx as usize);
                    }
                }
            }
            let holes = (0..n * n).filter(|&k| a[k] < 128 && !seen[k]).count();
            assert_eq!(holes, 0, "{name} has no holes (eyes)");
        }
        assert_eq!(shapes()[SHAPES.len() - 1].extras.len(), 2, "the pumpkin keeps its stem");
    }

    #[test]
    fn members_get_stable_varied_shapes() {
        assert_eq!(shape_of("coordinator"), shape_of("coordinator"));
        let names = ["coordinator", "graph", "bots", "theme", "quick", "tools", "agents-list", "verifier-ui", "verifier-core", "img-temp"];
        let mut picked: Vec<usize> = names.iter().map(|n| shape_of(n)).collect();
        picked.sort();
        picked.dedup();
        assert!(picked.len() >= 6, "{picked:?}");
    }
}
