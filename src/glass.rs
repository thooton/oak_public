//! The Liquid Glass theme (`ID`): Antigravity's colors (the dark Onyx theme
//! of its Hub, as its agent_ui_toolkit sets it), in a material after iOS's
//! Liquid Glass, ported from liquid_glass_widgets' lightweight shader (MIT,
//! pub.dev/packages/liquid_glass_widgets): frosted bodies that show what is
//! behind them, more saturated (its vibrancy), darker toward the bottom; a
//! hairline rim and inner bands of light along a light axis, brightest at
//! its two ends (its `rimLight`, a cos³ lobe at each); a sheen at the top.
//!
//! Oak draws as it always does: a theme is colors, its shapes square. At
//! the end of each frame (`Frame`'s drop) `finish` goes through what was
//! painted and turns the theme's surfaces into glass, each rectangle where
//! and as large as it was, its corners as they were: the page (`bg`)
//! becomes the backdrop (`backdrop`: soft glows of Antigravity's blues and
//! a violet over its near-black), and panels (`panel`, `panel2`, the tool
//! rows' two colors) glass over it. As the backdrop is Oak's own, a
//! surface's body is the backdrop under it, frosted and lit, drawn opaque:
//! it hides what is under it as the flat panel did, with no pass on the GPU
//! to read back the screen. Over Home's wallpaper the body is a blurred
//! copy of the picture instead (`wallpaper`, made once on a thread).
//! Shapes are matched by color (with the alpha a fade or a disabled widget
//! gives them, which the glass takes on), so plugins' pages turn to glass
//! too; colors the theme doesn't have stay as they are.

use crate::theme::Theme;
use eframe::egui::epaint::{Mesh, RectShape, TextureId, Vertex, WHITE_UV};
use eframe::egui::layers::ShapeIdx;
use eframe::egui::{self, Color32, CornerRadius, LayerId, Order, Pos2, Rect, Shape, Vec2, pos2, vec2};
use oak_sdk::theme::Palette;
use std::cell::RefCell;
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI};
use std::sync::{Arc, Mutex};

/// The theme's id in Settings, and its name.
pub(crate) const ID: &str = "glass";
pub(crate) const NAME: &str = "Liquid Glass";

const BG: u32 = 0x070708;
const PANEL: u32 = 0x131317;
const PANEL2: u32 = 0x1f1f24;
const ACCENT: u32 = 0x2f71e3;

/// Antigravity's dark Onyx: --background, --card, --onyx-selected,
/// --foreground, --secondary-foreground, --muted-foreground,
/// --placeholder, --primary, --error, and --border's white at 11%.
pub(crate) fn theme() -> Theme {
    Theme::from_palette(Palette {
        bg: BG,
        panel: PANEL,
        panel2: PANEL2,
        ink: 0xffffff,
        ink2: 0xbcbdc6,
        ink3: 0x87888f,
        ink4: 0x636470,
        accent: ACCENT,
        error: 0xde5555,
        line_alpha: 0x1c,
    })
}

/// `t` is this theme (a scope's look may have changed its other colors).
pub(crate) fn is(t: &Theme) -> bool {
    t.bg == oak_sdk::theme::hex(BG) && t.accent == oak_sdk::theme::hex(ACCENT)
}

// ------------------------------------------------------------- the frame

thread_local! {
    /// This frame's theme, while it is glass.
    static KIT: RefCell<Option<Kit>> = const { RefCell::new(None) };
    /// Home's wallpaper, when it was painted this frame.
    static WALL: RefCell<Option<Wall>> = const { RefCell::new(None) };
}

/// A frame drawn in `t`: when `t` is glass, what was painted turns to glass
/// as it ends (dropped, however the frame returns).
pub(crate) struct Frame {
    ctx: egui::Context,
}

impl Frame {
    pub(crate) fn begin(ctx: &egui::Context, t: &Theme) -> Frame {
        KIT.set(is(t).then(|| Kit::of(t, ctx.content_rect())));
        WALL.set(None);
        Frame { ctx: ctx.clone() }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        finish(&self.ctx);
    }
}

/// Glass is being drawn this frame.
pub(crate) fn active() -> bool {
    KIT.with_borrow(|k| k.is_some())
}

/// Home's wallpaper was painted (shape `idx` on `p`'s layer), over `rect`
/// with `uv`: glass over it shows its blurred copy, made from `source` (on
/// a thread, the first time `key` is seen).
pub(crate) fn wallpaper(
    p: &egui::Painter,
    idx: ShapeIdx,
    rect: Rect,
    uv: Rect,
    key: &str,
    source: impl FnOnce() -> Option<image::RgbaImage> + Send + 'static,
) {
    if !active() {
        return;
    }
    if let Some(tex) = frosted(p.ctx(), key, source) {
        WALL.set(Some(Wall { layer: p.layer_id(), idx: idx.0, rect, uv, tex }));
    }
}

struct Wall {
    layer: LayerId,
    idx: usize,
    rect: Rect,
    uv: Rect,
    tex: TextureId,
}

/// The blurred copies made, by key: None while one is made (or if it
/// couldn't be).
static FROSTED: Mutex<Option<HashMap<String, Option<egui::TextureHandle>>>> = Mutex::new(None);

fn frosted(ctx: &egui::Context, key: &str, source: impl FnOnce() -> Option<image::RgbaImage> + Send + 'static) -> Option<TextureId> {
    let mut all = FROSTED.lock().unwrap();
    let all = all.get_or_insert_default();
    if let Some(t) = all.get(key) {
        return t.as_ref().map(|t| t.id());
    }
    all.insert(key.to_string(), None);
    let (ctx, key) = (ctx.clone(), key.to_string());
    let _ = std::thread::Builder::new().name("glass-wallpaper".into()).spawn(move || {
        let Some(img) = source() else { return };
        let tex = ctx.load_texture(format!("glass-{key}"), frost_image(&img), egui::TextureOptions::LINEAR);
        if let Some(all) = FROSTED.lock().unwrap().as_mut() {
            all.insert(key, Some(tex));
        }
        ctx.request_repaint();
    });
    None
}

/// The picture as glass shows it behind: small (the texture's smooth
/// scaling does the rest of the blur), blurred, more saturated.
fn frost_image(img: &image::RgbaImage) -> egui::ColorImage {
    let w = 96u32.min(img.width().max(1));
    let h = ((img.height() as f32 * w as f32 / img.width().max(1) as f32).round() as u32).max(1);
    let small = image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle);
    let mut b = image::imageops::fast_blur(&small, 2.5);
    for p in b.pixels_mut() {
        let [r, g, bl] = saturate([p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0], SATURATION);
        p[0] = (r * 255.0).round() as u8;
        p[1] = (g * 255.0).round() as u8;
        p[2] = (bl * 255.0).round() as u8;
        p[3] = 255;
    }
    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], b.as_raw())
}

/// Turns what was painted this frame to glass (when it is drawn in glass).
fn finish(ctx: &egui::Context) {
    let Some(kit) = KIT.take() else { return };
    let wall = WALL.take();
    let mut layers: Vec<LayerId> = ctx.memory(|m| m.layer_ids().collect());
    if !layers.contains(&LayerId::background()) {
        layers.insert(0, LayerId::background());
    }
    ctx.graphics_mut(|g| {
        for layer in layers {
            let Some(list) = g.get_mut(layer) else { continue };
            for i in 0..list.next_idx().0 {
                // Over the wallpaper: after it in its layer, or in an area
                // above the page.
                let over = wall.as_ref().filter(|w| if w.layer == layer { i > w.idx } else { layer.order != Order::Background });
                list.mutate_shape(ShapeIdx(i), |s| kit.glass(&mut s.shape, over));
            }
        }
    });
}

// ------------------------------------------------------------ the optics

/// Saturation of what shows through (the package's iOS 27 dark: 1.4).
const SATURATION: f32 = 1.4;
/// BT.709 luma.
fn luma(c: [f32; 3]) -> f32 {
    c[0] * 0.2126 + c[1] * 0.7152 + c[2] * 0.0722
}

fn saturate(c: [f32; 3], s: f32) -> [f32; 3] {
    let l = luma(c);
    c.map(|v| (l + (v - l) * s).clamp(0.0, 1.0))
}

fn rgbf(h: u32) -> [f32; 3] {
    [((h >> 16) & 0xff) as f32 / 255.0, ((h >> 8) & 0xff) as f32 / 255.0, (h & 0xff) as f32 / 255.0]
}

/// The backdrop's glows: color, center (as a fraction of the window, may
/// be outside it), reach (of the window's longer side) and strength.
const GLOWS: [(u32, [f32; 2], f32, f32); 4] = [
    // --primary, from the top left.
    (0x2f71e3, [0.06, -0.10], 0.95, 0.34),
    // The logo's lighter blue, top right.
    (0x3a9cf0, [1.02, 0.05], 0.55, 0.14),
    // A violet, bottom right.
    (0x6a4fe0, [0.86, 1.12], 0.85, 0.20),
    // Light --primary, bottom left.
    (0x2560a3, [0.10, 1.05], 0.60, 0.12),
];

/// The light: from above, a little left (y grows down).
fn light() -> Vec2 {
    vec2(-0.28, -0.96).normalized()
}

/// How much light the rim catches where its outward normal is `n`: a cos³
/// lobe at each end of the light's axis, the far one at 85%.
fn lobe(n: Vec2) -> f32 {
    let along = n.dot(light());
    let (lit, opp) = (along.max(0.0), (-along).max(0.0));
    lit * lit * lit + 0.85 * opp * opp * opp
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// The page: the backdrop.
    Page,
    /// Glass, frosted by `lift`, its rim `rim` strong.
    Surface { lift: f32, rim: f32 },
    /// The accent's fill: lit, its color kept.
    Accent,
}

/// What the theme's colors become.
struct Kit {
    screen: Rect,
    bg: [u8; 3],
    surfaces: [([u8; 3], f32, f32); 4],
    accent: [u8; 3],
    /// The line's color and alpha, unmultiplied.
    line: ([u8; 3], u8),
    glows: [([f32; 3], [f32; 2], f32, f32); 4],
    base: [f32; 3],
}

/// A color's unmultiplied rgb and its alpha.
fn split(c: Color32) -> ([u8; 3], u8) {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    ([r, g, b], a)
}

/// `c` (unmultiplied, alpha `a`) is `want`, as far as premultiplying lets
/// it be told.
fn same(c: [u8; 3], a: u8, want: [u8; 3]) -> bool {
    if a < 8 {
        return false;
    }
    let tol = if a == 255 { 0 } else { 1 + 160 / a as i32 };
    c.iter().zip(want).all(|(x, y)| (*x as i32 - y as i32).abs() <= tol)
}

/// `c` at alpha `a`, premultiplied.
fn color(c: [f32; 3], a: f32) -> Color32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * a * 255.0).round() as u8;
    Color32::from_rgba_premultiplied(q(c[0]), q(c[1]), q(c[2]), (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn white(a: f32) -> Color32 {
    Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

impl Kit {
    fn of(t: &Theme, screen: Rect) -> Kit {
        let rgb = |c: Color32| split(c).0;
        Kit {
            screen,
            bg: rgb(t.bg),
            surfaces: [(rgb(t.tool_bg), 0.03, 0.0), (rgb(t.panel), 0.055, 1.0), (rgb(t.tool_hover), 0.075, 0.0), (rgb(t.panel2), 0.095, 1.0)],
            accent: rgb(t.accent),
            line: split(t.line),
            glows: GLOWS.map(|(c, at, r, k)| (rgbf(c), at, r, k)),
            base: rgbf(BG),
        }
    }

    /// What `c` is, and its alpha (0 to 1).
    fn kind(&self, c: Color32) -> Option<(Kind, f32)> {
        let (rgb, a) = split(c);
        let af = a as f32 / 255.0;
        if same(rgb, a, self.bg) {
            return Some((Kind::Page, af));
        }
        if let Some((_, lift, rim)) = self.surfaces.iter().find(|(s, ..)| same(rgb, a, *s)) {
            return Some((Kind::Surface { lift: *lift, rim: *rim }, af));
        }
        // (Faded, the accent is a selection's or a hover's tint: it stays.)
        (a > 230 && same(rgb, a, self.accent)).then_some((Kind::Accent, af))
    }

    fn is_line(&self, c: Color32) -> bool {
        let (rgb, a) = split(c);
        rgb == self.line.0 && (a as i32 - self.line.1 as i32).abs() <= 2
    }

    /// The backdrop at `p`.
    fn backdrop(&self, p: Pos2) -> [f32; 3] {
        let s = self.screen;
        let long = s.width().max(s.height()).max(1.0);
        let mut c = self.base;
        for (g, at, reach, k) in &self.glows {
            let center = pos2(s.left() + at[0] * s.width(), s.top() + at[1] * s.height());
            let d = (p - center).length() / (reach * long);
            let f = (1.0 - d * d).max(0.0);
            let f = f * f * k;
            for i in 0..3 {
                c[i] += g[i] * f;
            }
        }
        c.map(|v| v.min(1.0))
    }

    /// Glass over `b`, `v` of the way down it: more saturated, darkened
    /// (more toward the bottom), frosted toward a cool white by `lift`,
    /// with a sheen at the top.
    fn body(&self, b: [f32; 3], lift: f32, v: f32) -> [f32; 3] {
        let dark = 0.06 + 0.04 * v;
        let lift = lift + 0.03 * (1.0 - v) * (1.0 - v);
        let frost = [0.90, 0.93, 1.0];
        let s = saturate(b, SATURATION);
        [0, 1, 2].map(|i| {
            let c = s[i] * (1.0 - dark);
            c + (frost[i] - c) * lift
        })
    }

    /// Turns `shape` to glass when it is drawn in the theme's colors.
    fn glass(&self, shape: &mut Shape, wall: Option<&Wall>) {
        match shape {
            Shape::Vec(v) => v.iter_mut().for_each(|s| self.glass(s, wall)),
            Shape::Rect(r) if r.blur_width <= 0.0 && r.brush.is_none() => {
                if let Some(s) = self.rect(r, wall) {
                    *shape = s;
                }
            }
            Shape::Mesh(m) if m.texture_id == TextureId::default() => {
                if !m.vertices.iter().any(|v| self.kind(v.color).is_some_and(|(k, _)| k != Kind::Accent)) {
                    return;
                }
                // Over the wallpaper, a fade is of the glass over it.
                if let Some(w) = wall.filter(|w| w.rect.contains_rect(m.calc_bounds().shrink(0.5))) {
                    *shape = self.wall_fade(m, w);
                    return;
                }
                let m = Arc::make_mut(m);
                for v in &mut m.vertices {
                    v.color = self.recolor(v.color, v.pos);
                }
            }
            Shape::Circle(c) => {
                if let Some((Kind::Page, a)) = self.kind(c.fill) {
                    c.fill = color(self.backdrop(c.center), a);
                }
            }
            _ => {}
        }
    }

    /// A theme color at `p` in a mesh (a fade), as glass has it there.
    fn recolor(&self, c: Color32, p: Pos2) -> Color32 {
        match self.kind(c) {
            Some((Kind::Page, a)) => color(self.backdrop(p), a),
            Some((Kind::Surface { lift, .. }, a)) => color(self.body(self.backdrop(p), lift, 0.5), a),
            _ => c,
        }
    }

    /// `r` as glass: its body, its strokes, its rim, over the same
    /// rectangle with the same corners; None when it stays as it is.
    fn rect(&self, r: &RectShape, wall: Option<&Wall>) -> Option<Shape> {
        let rect = r.rect;
        if !rect.is_positive() {
            return None;
        }
        let line = r.stroke.width > 0.0 && self.is_line(r.stroke.color);
        let small = rect.width() < 4.0 || rect.height() < 4.0;
        let kind = self.kind(r.fill);
        // An outline in the line's color alone: a rim of glass.
        if kind.is_none() && line && r.fill.a() == 0 && !small && rect.width().min(rect.height()) >= 12.0 {
            let mut m = Mesh::default();
            rim(&mut m, rect, r.corner_radius, 0.8, r.stroke.color.a() as f32 / self.line.1.max(1) as f32);
            return Some(Shape::mesh(m));
        }
        let (kind, a) = kind?;
        if small {
            // A sliver (a line drawn as a rectangle): the color under it.
            return match kind {
                Kind::Page => {
                    let mut r = r.clone();
                    r.fill = color(self.backdrop(rect.center()), a);
                    Some(Shape::Rect(r))
                }
                _ => None,
            };
        }
        let wall = wall.filter(|w| w.rect.contains_rect(rect.shrink(0.5)));
        let mut out = Vec::with_capacity(4);
        let (lift, rim_k) = match kind {
            Kind::Accent => {
                let mut fill = r.clone();
                fill.stroke = egui::Stroke::NONE;
                out.push(Shape::Rect(fill));
                out.push(Shape::mesh(sheen(rect, r.corner_radius, a)));
                (None, 0.9)
            }
            // The page with a border is a box of the page's glass; over the
            // wallpaper, glass with no frost.
            Kind::Page if line || wall.is_some() => (Some(0.0), if line { 1.0 } else { 0.0 }),
            Kind::Page => (None, 0.0),
            Kind::Surface { lift, rim } => (Some(lift), if line { rim.max(0.8) } else { rim }),
        };
        match (kind, lift) {
            (Kind::Accent, _) => {}
            (_, Some(lift)) => match wall {
                Some(w) => out.extend(self.wall_body(r, w, lift, a)),
                None => out.push(Shape::mesh(body_mesh(rect, r.corner_radius, TextureId::default(), |p| {
                    let v = ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                    Vertex { pos: p, uv: WHITE_UV, color: color(self.body(self.backdrop(p), lift, v), a) }
                }))),
            },
            (_, None) => out.push(Shape::mesh(body_mesh(rect, r.corner_radius, TextureId::default(), |p| Vertex {
                pos: p,
                uv: WHITE_UV,
                color: color(self.backdrop(p), a),
            }))),
        }
        // Strokes other than the line (the accent's, a hover's) stay.
        if r.stroke.width > 0.0 && !line {
            let mut s = r.clone();
            s.fill = Color32::TRANSPARENT;
            out.push(Shape::Rect(s));
        }
        if rim_k > 0.0 {
            let mut m = Mesh::default();
            rim(&mut m, rect, r.corner_radius, rim_k, a);
            out.push(Shape::mesh(m));
        }
        Some(Shape::Vec(out))
    }

    /// A body over the wallpaper: its blurred copy, darkened, then frosted.
    fn wall_body(&self, r: &RectShape, w: &Wall, lift: f32, a: f32) -> [Shape; 2] {
        let rect = r.rect;
        let v_at = |p: Pos2| ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let pic = body_mesh(rect, r.corner_radius, w.tex, |p| Vertex { pos: p, uv: w.uv_at(p), color: wall_keep(v_at(p), a) });
        let frost = body_mesh(rect, r.corner_radius, TextureId::default(), |p| Vertex { pos: p, uv: WHITE_UV, color: self.wall_frost(lift, v_at(p), a) });
        [Shape::mesh(pic), Shape::mesh(frost)]
    }

    /// The frost over the wallpaper's copy, `v` of the way down: darker
    /// than over the backdrop (a picture is brighter), lighter for more
    /// `lift`.
    fn wall_frost(&self, lift: f32, v: f32, a: f32) -> Color32 {
        let k = ((0.40 + lift * 2.0 + 0.04 * (1.0 - v)) * a).clamp(0.0, 1.0);
        let q = |c: f32| (c * k * 255.0).round() as u8;
        Color32::from_rgba_premultiplied(q(self.base[0]), q(self.base[1]), q(self.base[2]), (k * 255.0).round() as u8)
    }

    /// A fade (a mesh in the theme's colors) over the wallpaper: the same
    /// triangles twice, the wallpaper's copy and its frost, each vertex as
    /// strong as its color was.
    fn wall_fade(&self, m: &Mesh, w: &Wall) -> Shape {
        let mut pic = Mesh::with_texture(w.tex);
        let mut frost = Mesh { indices: m.indices.clone(), ..Default::default() };
        pic.indices = m.indices.clone();
        for v in &m.vertices {
            let (lift, a) = match self.kind(v.color) {
                Some((Kind::Page, a)) => (0.0, a),
                Some((Kind::Surface { lift, .. }, a)) => (lift, a),
                _ => (0.0, 0.0),
            };
            pic.vertices.push(Vertex { pos: v.pos, uv: w.uv_at(v.pos), color: wall_keep(0.5, a) });
            frost.vertices.push(Vertex { pos: v.pos, uv: WHITE_UV, color: self.wall_frost(lift, 0.5, a) });
        }
        Shape::Vec(vec![Shape::mesh(pic), Shape::mesh(frost)])
    }
}

/// How much of the wallpaper's copy shows through, `v` of the way down a
/// body, at alpha `a`.
fn wall_keep(v: f32, a: f32) -> Color32 {
    color([1.0 - (0.10 + 0.06 * v); 3], a)
}

impl Wall {
    /// Where `p` is on the wallpaper's copy.
    fn uv_at(&self, p: Pos2) -> Pos2 {
        let f = (p - self.rect.min) / self.rect.size();
        pos2(self.uv.left() + f.x * self.uv.width(), self.uv.top() + f.y * self.uv.height())
    }
}

// ----------------------------------------------------------- geometry

/// How many steps round a corner as round as `cr`'s roundest.
fn steps(cr: CornerRadius) -> usize {
    let m = cr.nw.max(cr.ne).max(cr.se).max(cr.sw) as f32;
    if m < 0.5 { 1 } else { ((m / 2.5).ceil() as usize).clamp(2, 10) }
}

/// The edge of `rect` with corners `cr`, `inset` in, clockwise from the
/// top left: its points with their outward normals, `n` steps round each
/// corner (a square corner's point twice, once per side's normal).
fn ring(rect: Rect, cr: CornerRadius, inset: f32, n: usize) -> Vec<(Pos2, Vec2)> {
    let r = rect.shrink(inset);
    let most = (r.width().min(r.height()) / 2.0).max(0.0);
    let corners = [
        (cr.nw, r.left_top(), vec2(1.0, 1.0), PI),
        (cr.ne, r.right_top(), vec2(-1.0, 1.0), 1.5 * PI),
        (cr.se, r.right_bottom(), vec2(-1.0, -1.0), 0.0),
        (cr.sw, r.left_bottom(), vec2(1.0, -1.0), 0.5 * PI),
    ];
    let mut out = Vec::with_capacity(4 * (n + 1));
    for (c, corner, into, start) in corners {
        let rad = (c as f32 - inset).clamp(0.0, most);
        let center = corner + into * rad;
        for i in 0..=n {
            let a = start + FRAC_PI_2 * i as f32 / n as f32;
            let normal = vec2(a.cos(), a.sin());
            out.push((center + normal * rad, normal));
        }
    }
    out
}

/// A band between two rings of the same length, colored by each point's
/// normal.
fn band(m: &mut Mesh, outer: &[(Pos2, Vec2)], inner: &[(Pos2, Vec2)], co: impl Fn(Vec2) -> Color32, ci: impl Fn(Vec2) -> Color32) {
    let base = m.vertices.len() as u32;
    let k = outer.len().min(inner.len());
    for i in 0..k {
        m.colored_vertex(outer[i].0, co(outer[i].1));
        m.colored_vertex(inner[i].0, ci(inner[i].1));
    }
    for i in 0..k as u32 {
        let j = (i + 1) % k as u32;
        let (oi, ii, oj, ij) = (base + 2 * i, base + 2 * i + 1, base + 2 * j, base + 2 * j + 1);
        m.add_triangle(oi, oj, ij);
        m.add_triangle(oi, ij, ii);
    }
}

/// The rim, `k` strong at alpha `a`: a hairline brighter where the light
/// catches it, and inside it a band of light fading inward (the package's
/// rimLight core and tail, as one band).
fn rim(m: &mut Mesh, rect: Rect, cr: CornerRadius, k: f32, a: f32) {
    let n = steps(cr);
    let wide = (rect.width().min(rect.height()) * 0.25).clamp(2.0, 9.0);
    let (r0, r1, r2) = (ring(rect, cr, 0.0, n), ring(rect, cr, 1.0, n), ring(rect, cr, 1.0 + wide, n));
    let hair = |nrm: Vec2| white((0.07 + 0.24 * lobe(nrm)) * k * a);
    band(m, &r0, &r1, hair, hair);
    band(m, &r1, &r2, |nrm| white(0.11 * lobe(nrm) * k * a), |_| Color32::TRANSPARENT);
}

/// Light over the top half of an accent fill.
fn sheen(rect: Rect, cr: CornerRadius, a: f32) -> Mesh {
    let top = rect.top();
    let h = rect.height();
    body_mesh(rect, cr, TextureId::default(), |p| {
        let v = ((p.y - top) / h).clamp(0.0, 1.0);
        let k = (1.0 - v * 2.0).max(0.0);
        Vertex { pos: p, uv: WHITE_UV, color: white(0.16 * k * k * a) }
    })
}

/// A mesh filling `rect` with corners `cr`, each vertex from `vertex`: a
/// grid of cells at most 32 points wide for a square one (so the backdrop
/// under it shows), a fan for a round one.
fn body_mesh(rect: Rect, cr: CornerRadius, tex: TextureId, mut vertex: impl FnMut(Pos2) -> Vertex) -> Mesh {
    let mut m = Mesh::with_texture(tex);
    if cr == CornerRadius::ZERO {
        let nx = (rect.width() / 32.0).ceil().max(1.0) as u32;
        let ny = (rect.height() / 32.0).ceil().max(1.0) as u32;
        for j in 0..=ny {
            for i in 0..=nx {
                let p = pos2(rect.left() + rect.width() * i as f32 / nx as f32, rect.top() + rect.height() * j as f32 / ny as f32);
                m.vertices.push(vertex(p));
            }
        }
        for j in 0..ny {
            for i in 0..nx {
                let a = j * (nx + 1) + i;
                let (b, c, d) = (a + 1, a + nx + 1, a + nx + 2);
                m.add_triangle(a, b, d);
                m.add_triangle(a, d, c);
            }
        }
    } else {
        let edge = ring(rect, cr, 0.0, steps(cr));
        m.vertices.push(vertex(rect.center()));
        for (p, _) in &edge {
            m.vertices.push(vertex(*p));
        }
        let k = edge.len() as u32;
        for i in 0..k {
            m.add_triangle(0, 1 + i, 1 + (i + 1) % k);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kit() -> Kit {
        Kit::of(&theme(), Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0)))
    }

    /// The colors glass tells apart are all different, and each is told
    /// apart faded too.
    #[test]
    fn the_surfaces_are_told_apart() {
        let t = theme();
        let k = kit();
        let colors = [t.bg, t.tool_bg, t.panel, t.tool_hover, t.panel2, t.accent];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(k.kind(t.bg).map(|k| k.0), Some(Kind::Page));
        assert!(matches!(k.kind(t.panel), Some((Kind::Surface { rim, .. }, _)) if rim > 0.0));
        assert!(matches!(k.kind(t.panel.gamma_multiply(0.5)), Some((Kind::Surface { .. }, a)) if (a - 0.5).abs() < 0.01));
        assert_eq!(k.kind(t.ink), None);
        assert!(k.is_line(t.line));
        assert!(is(&t) && !is(&Theme::oak()));
    }

    /// Glass keeps every rectangle where and as large as it was: what it
    /// draws for one covers it and stays inside it.
    #[test]
    fn glass_keeps_the_shapes() {
        let t = theme();
        let k = kit();
        for (fill, stroke, radius) in [(t.bg, None, 0u8), (t.panel, Some(t.line), 0), (t.panel2, None, 4), (t.tool_bg, None, 0), (t.accent, None, 6), (Color32::TRANSPARENT, Some(t.line), 0)] {
            let rect = Rect::from_min_size(pos2(100.5, 40.0), vec2(300.0, 90.0));
            let mut r = RectShape::filled(rect, radius, fill);
            if let Some(c) = stroke {
                r.stroke = egui::Stroke::new(1.0, c);
            }
            let mut s = Shape::Rect(r);
            k.glass(&mut s, None);
            assert!(!matches!(s, Shape::Rect(ref r) if r.fill == fill), "{fill:?} turned to glass");
            let b = s.visual_bounding_rect();
            assert!(rect.expand(0.01).contains_rect(b), "{fill:?}: {b:?} inside {rect:?}");
            if fill != Color32::TRANSPARENT {
                assert!(b.expand(0.01).contains_rect(rect), "{fill:?}: {b:?} covers {rect:?}");
            }
        }
        // Text and colors the theme hasn't stay as they are.
        let mut other = Shape::Rect(RectShape::filled(Rect::from_min_size(Pos2::ZERO, vec2(50.0, 50.0)), 0, Color32::RED));
        k.glass(&mut other, None);
        assert!(matches!(other, Shape::Rect(ref r) if r.fill == Color32::RED));
    }

    /// A glass body is opaque (it hides what is under it, as the panel
    /// did), lighter than the page, and a fade's page color turns into the
    /// backdrop's.
    #[test]
    fn bodies_hide_what_is_under_them() {
        let t = theme();
        let k = kit();
        let mut s = Shape::Rect(RectShape::filled(Rect::from_min_size(pos2(10.0, 10.0), vec2(200.0, 100.0)), 0, t.panel));
        k.glass(&mut s, None);
        let Shape::Vec(parts) = s else { panic!("a panel turns into parts") };
        let Shape::Mesh(body) = &parts[0] else { panic!("its body is a mesh") };
        assert!(body.vertices.iter().all(|v| v.color.a() == 255));
        let page = k.backdrop(pos2(110.0, 60.0));
        let middle = body.vertices.iter().min_by(|a, b| (a.pos - pos2(110.0, 60.0)).length().total_cmp(&(b.pos - pos2(110.0, 60.0)).length())).unwrap();
        assert!(luma(rgbf(u32::from_be_bytes([0, middle.color.r(), middle.color.g(), middle.color.b()]))) > luma(page));
        let mut m = Mesh::default();
        m.colored_vertex(pos2(5.0, 5.0), t.bg);
        m.colored_vertex(pos2(50.0, 5.0), Color32::TRANSPARENT);
        m.colored_vertex(pos2(50.0, 50.0), Color32::TRANSPARENT);
        m.add_triangle(0, 1, 2);
        let mut fade = Shape::mesh(m);
        k.glass(&mut fade, None);
        let Shape::Mesh(m) = fade else { unreachable!() };
        assert_eq!(m.vertices[0].color, color(k.backdrop(pos2(5.0, 5.0)), 1.0));
        assert_eq!(m.vertices[1].color, Color32::TRANSPARENT);
    }

    /// The rim is brightest at the light's end, dimmest across it.
    #[test]
    fn the_rim_follows_the_light() {
        assert!(lobe(vec2(0.0, -1.0)) > 0.8);
        assert!(lobe(vec2(0.0, 1.0)) > 0.6);
        assert!(lobe(vec2(1.0, 0.0)) < 0.05);
    }
}
