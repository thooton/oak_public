//! The Liquid Glass theme (`ID`): Antigravity's colors (the dark Onyx theme
//! of its Hub, as its agent_ui_toolkit sets it) with the iOS 27 dark Liquid
//! Glass material of liquid_glass_widgets (MIT, pub.dev/packages/
//! liquid_glass_widgets), ported from its premium shader (`glass/`).
//!
//! As in the package, glass is for what floats over the content, and the
//! content's own cards stay solid: the theme's panels are glass in areas
//! above the page (the session box and the boxes in a chat's corner, menus,
//! popups, tooltips, dialogs and toasts), in the message box's row
//! (`chrome`) and over Home's wallpaper (`wallpaper`); on the page they are
//! the flat Onyx cards they are in its colors. A selected item (egui's
//! selection fill, `SELECT`) is the package's clear indicator lens, and a
//! switch held down lifts a lens over its knob.
//!
//! Oak draws as it always does: a theme is colors, its shapes square. At
//! the end of each frame (`Frame`'s drop) `finish` goes through what was
//! painted and puts a glass callback (`gpu`) where each glass rectangle
//! was, the same size, its corners as they were, over the flat panel (what
//! shows if the GPU part can't draw). Its stroke in the line's color is the
//! glass's rim now; the theme's colors on top of glass (a hovered row, a
//! button in a menu) turn into the package's fills inside glass, white at
//! a few percent. Shapes are matched by color, so plugins' pages turn to
//! glass the same way.
//!
//! Each glass refracts and frosts what was painted under it this frame:
//! glass that sees the same backdrop is a group (`groups`), and each
//! group's backdrop (the shapes before its first glass that reach its
//! region) is tessellated here for the GPU to draw before egui's pass.

mod gpu;

use crate::theme::Theme;
use eframe::egui::emath::TSTransform;
use eframe::egui::epaint::{ClippedPrimitive, ClippedShape, Primitive};
use eframe::egui::layers::ShapeIdx;
use eframe::egui::{self, Color32, CornerRadius, LayerId, Order, Pos2, Rect, Shape, Stroke, TextureId};
use eframe::{egui_wgpu, wgpu};
use oak_sdk::theme::{Palette, hex};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as Atomic};
use std::sync::{Arc, OnceLock};

/// The theme's id in Settings, and its name.
pub(crate) const ID: &str = "glass";
pub(crate) const NAME: &str = "Liquid Glass";

const BG: u32 = 0x070708;
const PANEL: u32 = 0x131317;
const PANEL2: u32 = 0x1f1f24;
const ACCENT: u32 = 0x2f71e3;

/// What egui fills a selected item with in this theme: a mark the frame's
/// end turns into the indicator lens (white at 10% where it can't).
pub(crate) const SELECT: Color32 = Color32::from_rgba_premultiplied(26, 26, 26, 26);

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
    t.bg == hex(BG) && t.accent == hex(ACCENT)
}

// ------------------------------------------------------------- the frame

thread_local! {
    /// This frame's theme, while it is glass.
    static KIT: RefCell<Option<Kit>> = const { RefCell::new(None) };
}

/// Frames laid out, for the GPU part to tell them apart.
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// A frame drawn in `t`: when `t` is glass, the frame's glass is laid out
/// as it ends (dropped, however the frame returns).
pub(crate) struct Frame {
    ctx: egui::Context,
}

impl Frame {
    pub(crate) fn begin(ctx: &egui::Context, t: &Theme, render: Option<&egui_wgpu::RenderState>) -> Frame {
        KIT.set(is(t).then(|| Kit::of(t, render.cloned())));
        Frame { ctx: ctx.clone() }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let Some(kit) = KIT.take() {
            kit.finish(&self.ctx);
        }
    }
}

/// Glass is being drawn this frame.
pub(crate) fn active() -> bool {
    KIT.with_borrow(|k| k.is_some())
}

/// Home's wallpaper was painted (shape `idx` on `p`'s layer, over `rect`):
/// the theme's surfaces painted over it are glass.
pub(crate) fn wallpaper(p: &egui::Painter, idx: ShapeIdx, rect: Rect) {
    KIT.with_borrow_mut(|k| {
        if let Some(k) = k {
            k.wallpaper = Some((p.layer_id(), idx.0, rect));
        }
    });
}

/// `rect` is chrome (the message box's row): the theme's surfaces in it are
/// glass, on the page too.
pub(crate) fn chrome(rect: Rect) {
    KIT.with_borrow_mut(|k| {
        if let Some(k) = k {
            k.chrome.push(rect);
        }
    });
}

// ----------------------------------------------------------- materials

/// The package's two materials.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Material {
    /// `LiquidGlassSettings.ios27Dark`: a frosted platter.
    Platter,
    /// `ios27Indicator`: the clear lens of a selected item.
    Lens,
}

impl Material {
    /// How far its backdrop reaches past it, in points: the frost's blur
    /// (3 sigma) and the lens's reach.
    fn pad(self) -> f32 {
        match self {
            Material::Platter => 3.0 * FROST + 8.0,
            Material::Lens => 16.0,
        }
    }
}

/// ios27Dark's frost: sigma in points, and how much white outweighs black.
const FROST: f32 = 14.0;
const FROST_WEIGHT: f32 = 0.5;

/// The shader's uniforms (`glass.wgsl`'s `Inst`), as floats.
pub(crate) const INST_FLOATS: usize = 44;

/// A glass rectangle found this frame.
#[derive(Clone, Debug)]
struct Spec {
    /// Its shape's place among all of the frame's (`Flat`).
    flat: usize,
    rect: Rect,
    radius: CornerRadius,
    material: Material,
    /// Its color's alpha (a fade's).
    alpha: f32,
    /// Pressed: where (points).
    touch: Option<Pos2>,
}

/// The uniforms of glass `s`, in a group whose backdrop texture starts at
/// `origin` and is `tex` large (pixels).
fn uniforms(s: &Spec, ppp: f32, origin: [u32; 2], tex: [u32; 2], screen_h: f32) -> [f32; INST_FLOATS] {
    let ds = ppp / 3.0;
    let r = s.rect;
    let cr = s.radius;
    let touch = s.touch.map_or([0.0; 3], |p| [p.x * ppp, p.y * ppp, 1.0]);
    let common = [
        r.left() * ppp,
        r.top() * ppp,
        r.right() * ppp,
        r.bottom() * ppp,
        origin[0] as f32,
        origin[1] as f32,
        tex[0] as f32,
        tex[1] as f32,
        cr.nw as f32 * ppp,
        cr.ne as f32 * ppp,
        cr.se as f32 * ppp,
        cr.sw as f32 * ppp,
    ];
    // glass, optics, light, rim, frost, misc; then touch and extra.
    let material: [f32; 24] = match s.material {
        // The light's angle is -pi/2, lit from the bottom: (cos, -sin).
        Material::Platter => [
            1.0, 1.0, 1.0, 31.0 / 255.0, // glassColor 0x1FFFFFFF
            1.24, 32.0 * ds, 1.4, 1.0, // refractiveIndex, thickness, saturation, paraxial
            0.0, 1.0, ds, s.alpha, // lightDirection, dprScale, opacity
            1.0, 1.15, 0.0, 0.035, // rimShade, rimLight, rimShadeEnds, edgeAbsorption
            0.85, -0.45, 0.6 * ppp, 2.5, // frostOpacity, frostClamp, blur (ghost), blurWeight
            0.28, 0.0, 0.0, (0.85 * ppp).max(1.5), // bodyShade, -, -, smoothing
        ],
        // Lit from the top: pi/2.
        Material::Lens => [
            1.0, 1.0, 1.0, 0.0, //
            1.17, 28.0 * ds, 1.0, 1.0, //
            0.0, -1.0, ds, s.alpha, //
            1.0, 1.0, 0.56, 0.0, //
            0.0, 0.0, 0.0, 1.0, //
            0.0, 1.0, 0.0, (1.25 * ppp).max(1.5),
        ],
    };
    let mut out = [0.0; INST_FLOATS];
    out[..12].copy_from_slice(&common);
    out[12..36].copy_from_slice(&material);
    out[36..40].copy_from_slice(&[touch[0], touch[1], touch[2], 0.0]);
    out[40..44].copy_from_slice(&[screen_h, 0.0, 0.0, 0.0]);
    out
}

/// A frame's glass, for the GPU part.
pub(crate) struct Job {
    id: u64,
    ppp: f32,
    screen: [u32; 2],
    target: wgpu::TextureFormat,
    groups: Vec<GroupJob>,
    insts: Vec<[f32; INST_FLOATS]>,
    inst_group: Vec<usize>,
    /// egui's textures the backdrops use.
    textures: Vec<(TextureId, wgpu::Texture, egui::TextureOptions)>,
}

struct GroupJob {
    /// What to draw into the scene before the group's glass: what was
    /// painted since the group before's.
    prims: Vec<ClippedPrimitive>,
    /// The scene's part the group reads: x, y, width, height (pixels).
    region: [u32; 4],
    /// Its textures' size: the region's, rounded up to 64.
    tex: [u32; 2],
    /// The cloud's blur, in the cloud's pixels (a quarter's).
    sigma: f32,
    weight: f32,
}

/// A group of glass seeing the same backdrop: the shapes before `cut`.
#[derive(Debug)]
struct Group {
    cut: usize,
    specs: Vec<usize>,
    /// Where its backdrop is read (points).
    area: Rect,
}

/// At most this many backdrops a frame: past it glass joins the last.
const MAX_GROUPS: usize = 8;

/// The glass in `specs` (in painting order) as groups: each joins the last
/// group unless something painted since that group's cut (`bounds`, all
/// of the frame's shapes in order) reaches where it reads.
fn groups(specs: &[Spec], bounds: &[Rect]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let reach = s.rect.expand(s.material.pad());
        let near = s.rect.expand(s.material.pad() * 0.5);
        let join = out.last().is_some_and(|g| out.len() >= MAX_GROUPS || bounds[g.cut..s.flat].iter().all(|b| !b.intersects(near)));
        match out.last_mut() {
            Some(g) if join => {
                g.specs.push(i);
                g.area = g.area.union(reach);
            }
            _ => out.push(Group { cut: s.flat, specs: vec![i], area: reach }),
        }
    }
    out
}

// ---------------------------------------------------------- the colors

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// The page.
    Page,
    /// The tool rows' color, the panel, a hovered row, panel2: in order.
    Surface(u8),
    /// A selected item.
    Select,
}

/// What the theme's colors become.
struct Kit {
    bg: [u8; 3],
    surfaces: [[u8; 3]; 4],
    line: ([u8; 3], u8),
    render: Option<egui_wgpu::RenderState>,
    wallpaper: Option<(LayerId, usize, Rect)>,
    chrome: Vec<Rect>,
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

/// White at `a`, the package's fill inside glass for a surface's color.
fn white(a: f32) -> Color32 {
    Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Where a shape is painted.
struct Place<'a> {
    floating: bool,
    over_wallpaper: bool,
    chrome: &'a [Rect],
    /// The layer's glass so far.
    glass: &'a [Rect],
}

impl Kit {
    fn of(t: &Theme, render: Option<egui_wgpu::RenderState>) -> Kit {
        let rgb = |c: Color32| split(c).0;
        Kit {
            bg: rgb(t.bg),
            surfaces: [rgb(t.tool_bg), rgb(t.panel), rgb(t.tool_hover), rgb(t.panel2)],
            line: split(t.line),
            render,
            wallpaper: None,
            chrome: Vec::new(),
        }
    }

    /// What `c` is, and its alpha (0 to 1).
    fn kind(&self, c: Color32) -> Option<(Kind, f32)> {
        if c == SELECT {
            return Some((Kind::Select, 1.0));
        }
        let (rgb, a) = split(c);
        let af = a as f32 / 255.0;
        if same(rgb, a, self.bg) {
            return Some((Kind::Page, af));
        }
        let i = self.surfaces.iter().position(|s| same(rgb, a, *s))?;
        Some((Kind::Surface(i as u8), af))
    }

    fn is_line(&self, c: Color32) -> bool {
        let (rgb, a) = split(c);
        rgb == self.line.0 && (a as i32 - self.line.1 as i32).abs() <= 2
    }

    /// The fill inside glass for a theme color: white at a few percent for
    /// a surface, a dark well for the page; None for other colors.
    fn inside(&self, c: Color32) -> Option<Color32> {
        Some(match self.kind(c)? {
            (Kind::Page, a) => Color32::from_black_alpha((0.35 * a * 255.0) as u8),
            (Kind::Surface(i), a) => white([0.035, 0.055, 0.075, 0.095][i as usize] * a),
            (Kind::Select, _) => return None,
        })
    }

    /// The theme's colors in `shape`, painted on glass, as fills inside it.
    fn recolor_inside(&self, shape: &mut Shape) {
        match shape {
            Shape::Vec(v) => v.iter_mut().for_each(|s| self.recolor_inside(s)),
            Shape::Rect(r) => {
                if let Some(c) = self.inside(r.fill) {
                    r.fill = c;
                }
            }
            Shape::Mesh(m) if m.texture_id == TextureId::default() && m.vertices.iter().any(|v| self.inside(v.color).is_some()) => {
                for v in &mut Arc::make_mut(m).vertices {
                    if let Some(c) = self.inside(v.color) {
                        v.color = c;
                    }
                }
            }
            _ => {}
        }
    }

    /// `shape`, painted at `place`: turned to glass (a callback for glass
    /// number `inst` of the frame's `job`) when it is a glass rectangle,
    /// and what that glass is; recolored when it is on glass.
    fn classify(&self, shape: &mut Shape, place: &Place, job: &Arc<OnceLock<Job>>, inst: usize) -> Option<(Rect, CornerRadius, Material, f32)> {
        let bounds = shape.visual_bounding_rect();
        let on_glass = place.glass.iter().any(|g| g.expand(0.5).contains_rect(bounds));
        if let Shape::Rect(r) = shape
            && r.blur_width <= 0.0
            && r.brush.is_none()
            && let Some((kind, alpha)) = self.kind(r.fill)
        {
            let rect = r.rect;
            let line = r.stroke.width > 0.0 && self.is_line(r.stroke.color);
            let big = rect.width() >= 10.0 && rect.height() >= 10.0;
            let in_chrome = place.chrome.iter().any(|c| c.expand(0.5).contains_rect(rect));
            let glass = big
                && match kind {
                    Kind::Select => true,
                    _ if on_glass => false,
                    Kind::Page => place.floating || ((place.over_wallpaper || in_chrome) && line),
                    Kind::Surface(_) => place.floating || place.over_wallpaper || in_chrome,
                };
            if glass {
                let material = if kind == Kind::Select { Material::Lens } else { Material::Platter };
                let mut flat = r.clone();
                flat.stroke = Stroke::NONE;
                let callback = egui_wgpu::Callback::new_paint_callback(rect, gpu::GlassCallback { job: job.clone(), inst });
                let mut parts = vec![Shape::Rect(flat), Shape::Callback(callback)];
                // A stroke other than the line's (a focus ring) stays.
                if r.stroke.width > 0.0 && !line {
                    let mut ring = r.clone();
                    ring.fill = Color32::TRANSPARENT;
                    parts.push(Shape::Rect(ring));
                }
                let out = (rect, r.corner_radius, material, alpha);
                *shape = Shape::Vec(parts);
                return Some(out);
            }
        }
        if on_glass {
            self.recolor_inside(shape);
        }
        None
    }

    /// Lays the frame's glass out: puts its callbacks in, and the job they
    /// draw from.
    fn finish(self, ctx: &egui::Context) {
        let Some(render) = self.render.clone() else { return };
        let ppp = ctx.pixels_per_point();
        let screen_pt = ctx.content_rect();
        let screen = [(screen_pt.width() * ppp).round() as u32, (screen_pt.height() * ppp).round() as u32];
        if screen[0] == 0 || screen[1] == 0 {
            return;
        }
        let (pointer, down) = ctx.input(|i| (i.pointer.interact_pos(), i.pointer.primary_down()));
        let (order, transforms): (Vec<LayerId>, HashMap<LayerId, TSTransform>) =
            ctx.memory(|m| (m.layer_ids().collect(), m.to_global.iter().map(|(k, v)| (*k, *v)).collect()));
        // egui's painting order: by Order, the areas' order in each, then
        // the layers outside it (the page's).
        let mut layers = Vec::new();
        for o in [Order::Background, Order::Middle, Order::Foreground, Order::Tooltip, Order::Debug] {
            layers.extend(order.iter().filter(|l| l.order == o));
            if o == Order::Background && !order.contains(&LayerId::background()) {
                layers.push(LayerId::background());
            }
        }

        let job = Arc::new(OnceLock::new());
        let mut specs: Vec<Spec> = Vec::new();
        let mut bounds: Vec<Rect> = Vec::new();
        ctx.graphics_mut(|g| {
            for layer in &layers {
                let Some(list) = g.get_mut(*layer) else { continue };
                let transform = transforms.get(layer).copied();
                let wall = self.wallpaper.filter(|w| w.0 == *layer);
                let mut glass: Vec<Rect> = Vec::new();
                for i in 0..list.next_idx().0 {
                    let mut found = None;
                    let mut bound = Rect::NOTHING;
                    list.mutate_shape(ShapeIdx(i), |cs| {
                        let b = cs.shape.visual_bounding_rect().intersect(cs.clip_rect);
                        bound = transform.map_or(b, |t| t * b);
                        // A layer moved or scaled stays as it is drawn.
                        if transform.is_some() {
                            return;
                        }
                        let place = Place {
                            floating: layer.order != Order::Background,
                            over_wallpaper: wall.is_some_and(|w| i > w.1 && w.2.contains_rect(b)),
                            chrome: &self.chrome,
                            glass: &glass,
                        };
                        found = self.classify(&mut cs.shape, &place, &job, specs.len());
                    });
                    if let Some((rect, radius, material, alpha)) = found {
                        glass.push(rect);
                        let touch = pointer.filter(|p| down && rect.contains(*p));
                        specs.push(Spec { flat: bounds.len(), rect, radius, material, alpha, touch });
                    }
                    bounds.push(bound);
                }
            }
        });
        if specs.is_empty() {
            return;
        }

        // Each group's backdrop: the shapes since the group before's cut
        // that reach it or a later group (the scene keeps them).
        let groups = groups(&specs, &bounds);
        let mut reach = vec![Rect::NOTHING; groups.len()];
        let mut acc = Rect::NOTHING;
        for (i, g) in groups.iter().enumerate().rev() {
            acc = acc.union(g.area);
            reach[i] = acc;
        }
        let mut shapes: Vec<Vec<ClippedShape>> = vec![Vec::new(); groups.len()];
        ctx.graphics(|g| {
            let mut k = 0;
            let mut gi = 0;
            for layer in &layers {
                let Some(list) = g.get(*layer) else { continue };
                let transform = transforms.get(layer).copied();
                for cs in list.all_entries() {
                    while gi < groups.len() && k >= groups[gi].cut {
                        gi += 1;
                    }
                    if gi < groups.len() && bounds[k].intersects(reach[gi]) {
                        let mut cs = cs.clone();
                        if let Some(t) = transform {
                            cs.transform(t);
                        }
                        shapes[gi].push(cs);
                    }
                    k += 1;
                }
            }
        });

        // Tessellated, with egui's textures they use (as its renderer has
        // them; those it hasn't yet are left out).
        let mut prims: Vec<Vec<ClippedPrimitive>> = shapes.into_iter().map(|s| ctx.tessellate(s, ppp)).collect();
        let mut textures = Vec::new();
        {
            let renderer = render.renderer.read();
            let mut seen = HashMap::new();
            for list in &mut prims {
                list.retain(|p| match &p.primitive {
                    Primitive::Mesh(m) => *seen.entry(m.texture_id).or_insert_with(|| match renderer.texture(&m.texture_id) {
                        Some(egui_wgpu::Texture { texture: Some(t), options, .. }) => {
                            textures.push((m.texture_id, t.clone(), options.unwrap_or_default()));
                            true
                        }
                        _ => false,
                    }),
                    Primitive::Callback(_) => true,
                });
            }
            // A font atlas that grew is drawn from the old one until egui's
            // renderer has the new: look again next frame.
            let font = TextureId::default();
            if let Some((_, t, _)) = textures.iter().find(|(id, ..)| *id == font)
                && let Some(meta) = ctx.tex_manager().read().meta(font)
                && (t.width() as usize, t.height() as usize) != (meta.size[0], meta.size[1])
            {
                ctx.request_repaint();
            }
        }

        let to_px = |r: Rect| {
            let x0 = (r.left() * ppp).floor().clamp(0.0, screen[0] as f32) as u32;
            let y0 = (r.top() * ppp).floor().clamp(0.0, screen[1] as f32) as u32;
            let x1 = (r.right() * ppp).ceil().clamp(0.0, screen[0] as f32) as u32;
            let y1 = (r.bottom() * ppp).ceil().clamp(0.0, screen[1] as f32) as u32;
            [x0, y0, (x1 - x0).max(1), (y1 - y0).max(1)]
        };
        let mut inst_group = vec![0; specs.len()];
        let mut insts = vec![[0.0; INST_FLOATS]; specs.len()];
        let mut jobs = Vec::with_capacity(groups.len());
        for ((gi, g), prims) in groups.iter().enumerate().zip(prims) {
            let region = to_px(g.area);
            let tex = [region[2].div_ceil(64) * 64, region[3].div_ceil(64) * 64];
            for &s in &g.specs {
                inst_group[s] = gi;
                insts[s] = uniforms(&specs[s], ppp, [region[0], region[1]], tex, screen[1] as f32);
            }
            jobs.push(GroupJob { prims, region, tex, sigma: FROST * ppp / 4.0, weight: FROST_WEIGHT });
        }
        let _ = job.set(Job {
            id: FRAMES.fetch_add(1, Atomic::Relaxed) + 1,
            ppp,
            screen,
            target: render.target_format,
            groups: jobs,
            insts,
            inst_group,
            textures,
        });
    }
}

/// A shape's vertices in a color (for the tests).
#[cfg(test)]
fn colors(s: &Shape) -> Vec<Color32> {
    match s {
        Shape::Vec(v) => v.iter().flat_map(colors).collect(),
        Shape::Rect(r) => vec![r.fill],
        Shape::Mesh(m) => m.vertices.iter().map(|v| v.color).collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::epaint::RectShape;
    use eframe::egui::{pos2, vec2};

    fn kit() -> Kit {
        Kit::of(&theme(), None)
    }

    fn place<'a>(floating: bool, chrome: &'a [Rect], glass: &'a [Rect]) -> Place<'a> {
        Place { floating, over_wallpaper: false, chrome, glass }
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(x, y), vec2(w, h))
    }

    /// The colors glass tells apart are all different, and each is told
    /// apart faded too.
    #[test]
    fn the_colors_are_told_apart() {
        let t = theme();
        let k = kit();
        let colors = [t.bg, t.tool_bg, t.panel, t.tool_hover, t.panel2, t.accent, SELECT];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(k.kind(t.bg).map(|k| k.0), Some(Kind::Page));
        assert_eq!(k.kind(t.panel).map(|k| k.0), Some(Kind::Surface(1)));
        assert!(matches!(k.kind(t.panel.gamma_multiply(0.5)), Some((Kind::Surface(1), a)) if (a - 0.5).abs() < 0.01));
        assert_eq!(k.kind(SELECT).map(|k| k.0), Some(Kind::Select));
        assert_eq!(k.kind(t.ink), None);
        assert_eq!(k.kind(t.accent), None);
        assert!(k.is_line(t.line));
        assert!(is(&t) && !is(&Theme::oak()));
    }

    /// A panel floating over the page is glass where it was, its flat
    /// panel under the callback; on the page it stays a card; in the
    /// message box's row it is glass again.
    #[test]
    fn what_turns_to_glass() {
        let t = theme();
        let k = kit();
        let job = Arc::new(OnceLock::new());
        let r = rect(10.0, 10.0, 200.0, 80.0);
        let mut panel = RectShape::filled(r, 0, t.panel);
        panel.stroke = Stroke::new(1.0, t.line);
        let mut s = Shape::Rect(panel.clone());
        let found = k.classify(&mut s, &place(true, &[], &[]), &job, 0);
        assert_eq!(found.map(|f| (f.0, f.2)), Some((r, Material::Platter)));
        let Shape::Vec(parts) = &s else { panic!("glass is the panel and its callback") };
        assert!(matches!(&parts[0], Shape::Rect(f) if f.rect == r && f.fill == t.panel && f.stroke.is_empty()));
        assert!(matches!(&parts[1], Shape::Callback(c) if c.rect == r));
        assert_eq!(parts.len(), 2, "the line's stroke is the rim now");

        let mut card = Shape::Rect(panel.clone());
        assert!(k.classify(&mut card, &place(false, &[], &[]), &job, 0).is_none());
        assert!(matches!(card, Shape::Rect(ref c) if c.fill == t.panel), "a card on the page stays");

        let chrome = [r.expand(4.0)];
        assert!(k.classify(&mut Shape::Rect(panel.clone()), &place(false, &chrome, &[]), &job, 0).is_some());
        // The page's own fill in the row isn't glass.
        assert!(k.classify(&mut Shape::Rect(RectShape::filled(r, 0, t.bg)), &place(false, &chrome, &[]), &job, 0).is_none());

        // A selected item is the lens anywhere.
        let mut sel = Shape::Rect(RectShape::filled(rect(20.0, 20.0, 60.0, 20.0), 0, SELECT));
        assert_eq!(k.classify(&mut sel, &place(false, &[], &[]), &job, 0).map(|f| f.2), Some(Material::Lens));

        // Too small, or not the theme's color: as it was.
        assert!(k.classify(&mut Shape::Rect(RectShape::filled(rect(0.0, 0.0, 6.0, 40.0), 0, t.panel)), &place(true, &[], &[]), &job, 0).is_none());
        let mut red = Shape::Rect(RectShape::filled(r, 0, Color32::RED));
        assert!(k.classify(&mut red, &place(true, &[], &[]), &job, 0).is_none());
        assert!(matches!(red, Shape::Rect(ref c) if c.fill == Color32::RED));
    }

    /// The theme's colors on glass are the package's fills inside it: a
    /// hovered row white at a few percent, not a solid panel.
    #[test]
    fn surfaces_on_glass_are_fills() {
        let t = theme();
        let k = kit();
        let job = Arc::new(OnceLock::new());
        let glass = [rect(0.0, 0.0, 300.0, 200.0)];
        let mut row = Shape::Rect(RectShape::filled(rect(10.0, 10.0, 200.0, 24.0), 0, t.tool_hover));
        assert!(k.classify(&mut row, &place(true, &[], &glass), &job, 1).is_none());
        let c = colors(&row)[0];
        assert!(c.r() == c.g() && c.g() == c.b() && c.a() > 0 && c.a() < 40, "{c:?}");
        // Text and other colors are left alone.
        let mut other = Shape::Rect(RectShape::filled(rect(10.0, 40.0, 50.0, 20.0), 0, t.accent));
        k.classify(&mut other, &place(true, &[], &glass), &job, 1);
        assert_eq!(colors(&other), vec![t.accent]);
    }

    /// Glass seeing the same backdrop shares a group; glass under which
    /// something was painted since starts one.
    #[test]
    fn glass_is_grouped_by_backdrop() {
        let spec = |flat: usize, r: Rect| Spec { flat, rect: r, radius: CornerRadius::ZERO, material: Material::Platter, alpha: 1.0, touch: None };
        let a = rect(0.0, 0.0, 100.0, 40.0);
        let far = rect(600.0, 0.0, 100.0, 40.0);
        let below = rect(0.0, 48.0, 100.0, 40.0);
        // 0: page, 1: glass a, 2: a's text, 3: glass far, 4: glass below a.
        let bounds = [rect(0.0, 0.0, 800.0, 600.0), a, a.shrink(8.0), far, below];
        let specs = [spec(1, a), spec(3, far), spec(4, below)];
        let g = groups(&specs, &bounds);
        assert_eq!(g.iter().map(|g| (g.cut, g.specs.clone())).collect::<Vec<_>>(), vec![(1, vec![0, 1]), (4, vec![2])]);
        assert!(g[0].area.contains_rect(a.expand(FROST)) && g[0].area.contains_rect(far));
    }

    /// The uniforms say where the glass is, in pixels, and its material.
    #[test]
    fn uniforms_follow_the_package() {
        let s = Spec { flat: 0, rect: rect(10.0, 20.0, 100.0, 50.0), radius: CornerRadius::same(4), material: Material::Platter, alpha: 1.0, touch: None };
        let u = uniforms(&s, 2.0, [0, 0], [256, 256], 800.0);
        assert_eq!(&u[0..4], &[20.0, 40.0, 220.0, 140.0]);
        assert_eq!(&u[8..12], &[8.0; 4]);
        assert!((u[17] - 32.0 * 2.0 / 3.0).abs() < 1e-4, "thickness 32 at 3x");
        assert_eq!(&u[20..22], &[0.0, 1.0], "ios27Dark is lit from the bottom");
        assert_eq!(&u[28..30], &[0.85, -0.45]);
    }

    /// The shaders are valid WGSL.
    #[test]
    fn the_shaders_parse() {
        for (name, src) in [("glass", include_str!("glass/glass.wgsl")), ("blur", include_str!("glass/blur.wgsl"))] {
            let module = wgpu::naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
            wgpu::naga::valid::Validator::new(wgpu::naga::valid::ValidationFlags::all(), wgpu::naga::valid::Capabilities::all())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
    }
}
