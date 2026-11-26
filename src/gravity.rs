//! The Antigravity theme (`ID`): Oak laid out the way Google Antigravity's
//! desktop Hub is, in a palette drawn from it. Unlike Oak's other themes it
//! changes shapes and layout as well as colors:
//!
//! - the window is a shell (`SHELL`, the Hub's `--shell`/`--sidebar`) with
//!   the page in an inset, rounded pane (`pane`, its corners masked so
//!   whatever the page paints is cut to them, `mask_pane`);
//! - the tab strip is a sidebar at the left (`gravity_side.rs`): a New
//!   conversation pill, Home and Usage, the open tabs as rows, Settings at
//!   the foot;
//! - corners are round (`round`, `radius`): egui's widgets, menus and
//!   windows, and the boxes Oak paints itself (the message box, the corner
//!   boxes, buttons, cards, dialogs, switches);
//! - Home is a desktop (`gravity_home.rs`): tiles (a clock, a calendar, an
//!   overview, the recent conversations, the agents at work, usage) over
//!   the dimmed wallpaper, and a dock at the bottom in place of the corner
//!   buttons.
//!
//! The palette is the Hub's dark Onyx moved a little toward blue, with its
//! logo's gradient (blue, green, yellow, orange, red: `BRAND`) for accents:
//! the logo itself (`paint_logo`) and the clock.

use crate::theme::Theme;
use eframe::egui::emath::TSTransform;
use eframe::egui::{self, Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Stroke, vec2};
use oak_sdk::theme::{Palette, hex, mix};

/// The theme's id in Settings, and its name.
pub(crate) const ID: &str = "antigravity";
pub(crate) const NAME: &str = "Antigravity";

/// The page's pane.
const BG: u32 = 0x0b0c10;
/// What the pane sits in: the title bar, the sidebar, the pane's margin.
pub(crate) const SHELL_HEX: u32 = 0x131419;
const PANEL: u32 = 0x17181e;
const PANEL2: u32 = 0x22242c;
const ACCENT: u32 = 0x4d8dff;

/// The shell's color.
pub(crate) fn shell() -> Color32 {
    hex(SHELL_HEX)
}

/// A selected row in the sidebar (the Hub's `--onyx-selected`, bluer).
pub(crate) fn selected() -> Color32 {
    hex(0x23252e)
}

/// The logo's gradient, from its foot to its crown.
pub(crate) const BRAND: [u32; 6] = [0x4d8dff, 0x2fb4c8, 0x6cc46a, 0xedc232, 0xf7902c, 0xf0503c];

/// The four colors the desktop's tiles are marked with.
pub(crate) const MARKS: [u32; 4] = [0x4d8dff, 0x6cc46a, 0xedc232, 0xf0503c];

/// The pane's corners.
pub(crate) const PANE_R: f32 = 14.0;
/// Between the pane and the window's edges.
pub(crate) const PANE_MARGIN: f32 = 8.0;
/// The sidebar's width.
pub(crate) const SIDE_W: f32 = 236.0;

/// The Hub's dark Onyx, a little bluer: page, card, raised, foreground,
/// secondary and muted foreground, placeholder, primary and error, and the
/// border as white at 10%.
pub(crate) fn theme() -> Theme {
    Theme::from_palette(Palette {
        bg: BG,
        panel: PANEL,
        panel2: PANEL2,
        ink: 0xf5f6fa,
        ink2: 0xbfc1cb,
        ink3: 0x8a8c96,
        ink4: 0x5f616c,
        accent: ACCENT,
        error: 0xf0564a,
        line_alpha: 0x1a,
    })
}

/// `t` is this theme (a scope's look may have changed its other colors).
pub(crate) fn is(t: &Theme) -> bool {
    t.bg == hex(BG) && t.accent == hex(ACCENT)
}

/// `r` in this theme, square (0) in the others: the radius of a box Oak
/// paints itself.
pub(crate) fn round(t: &Theme, r: f32) -> f32 {
    if is(t) { r } else { 0.0 }
}

/// `round` as egui's corner radius.
pub(crate) fn radius(t: &Theme, r: f32) -> CornerRadius {
    CornerRadius::same(round(t, r).round() as u8)
}

/// The brand gradient at `f` (0 its foot, blue; 1 its crown, red).
pub(crate) fn brand_at(f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0) * (BRAND.len() - 1) as f32;
    let i = (f.floor() as usize).min(BRAND.len() - 2);
    mix(hex(BRAND[i]), hex(BRAND[i + 1]), f - i as f32)
}

/// An arc round `c` of radius `r`, from angle `a0` to `a1` (radians, 0 at
/// the top, clockwise), `w` thick, in the brand gradient along it, with
/// soft edges.
pub(crate) fn gradient_arc(p: &Painter, c: Pos2, r: f32, a0: f32, a1: f32, w: f32, alpha: f32) {
    let n = (((a1 - a0).abs() * r / 3.0).ceil() as usize).clamp(8, 256);
    let mut m = Mesh::default();
    for i in 0..=n {
        let f = i as f32 / n as f32;
        let a = a0 + (a1 - a0) * f;
        let d = vec2(a.sin(), -a.cos());
        let col = brand_at(f).gamma_multiply(alpha);
        for (off, solid) in [
            (w / 2.0 + 0.8, false),
            (w / 2.0 - 0.4, true),
            (-(w / 2.0 - 0.4), true),
            (-(w / 2.0 + 0.8), false),
        ] {
            m.colored_vertex(
                c + d * (r + off),
                if solid { col } else { Color32::TRANSPARENT },
            );
        }
        if i > 0 {
            let b = (i as u32 - 1) * 4;
            for k in 0..3 {
                m.add_triangle(b + k, b + k + 1, b + 4 + k);
                m.add_triangle(b + k + 1, b + 4 + k, b + 5 + k);
            }
        }
    }
    p.add(m);
}

/// Antigravity's arch in `rect` (square): a bell curve, thick at its crown
/// and thin at its feet, blue at the feet warming to green and yellow on
/// the left of the crown and orange and red on its right; in `mono` (the
/// faint one, when given) all of it that color.
pub(crate) fn paint_logo(p: &Painter, rect: Rect, mono: Option<Color32>) {
    let s = rect.width().min(rect.height());
    let o = rect.center() - vec2(s, s) / 2.0;
    let at = |x: f32| {
        let g = (-((x - 0.5) / 0.15).powi(2)).exp();
        egui::pos2(x, 0.80 - 0.56 * g)
    };
    let n = 56;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| at(0.17 + 0.66 * i as f32 / n as f32))
        .collect();
    let mut m = Mesh::default();
    let feather = 0.9 / s;
    for (i, &c) in pts.iter().enumerate() {
        let prev = pts[i.saturating_sub(1)];
        let next = pts[(i + 1).min(n)];
        let tan = (next - prev).normalized();
        let nrm = vec2(-tan.y, tan.x);
        let g = (-((c.x - 0.5) / 0.14).powi(2)).exp();
        // Thick at the crown, thinning to the feet, which taper off.
        let end = ((i.min(n - i)) as f32 / 4.0).min(1.0);
        let half = (0.028 + 0.062 * g) * (0.45 + 0.55 * end);
        let col = mono.unwrap_or_else(|| {
            let heat = ((0.80 - c.y) / 0.56).clamp(0.0, 1.0).powf(1.6);
            let warm = brand_at(0.45 + (c.x - 0.38) * 2.4);
            mix(hex(BRAND[0]), warm, heat)
        });
        for (off, solid) in [
            (half + feather, false),
            (half, true),
            (-half, true),
            (-half - feather, false),
        ] {
            let q = c + nrm * off;
            m.colored_vertex(
                o + vec2(q.x, q.y) * s,
                if solid { col } else { Color32::TRANSPARENT },
            );
        }
        if i > 0 {
            let b = (i as u32 - 1) * 4;
            for k in 0..3 {
                m.add_triangle(b + k, b + k + 1, b + 4 + k);
                m.add_triangle(b + k + 1, b + 4 + k, b + 5 + k);
            }
        }
    }
    p.add(m);
}

// ------------------------------------------------------- edge lighting
//
// Antigravity's boxes and bubbles are edged as its app's are (measured from
// its message box, at 1:1): a crisp one-pixel hairline of white round the
// box, a little stronger along the top (4.4% over the box's fill) than down
// the sides (3.0%) and along the bottom (2.6%), and a second, lighter line
// (7.7%) just inside the top edge, which follows the top corners' curve and
// fades out as it turns down the sides, and a faint shadow (black at 23%) in
// the pixel under the bottom edge, fading out round the bottom corners. The body stays flat, and the
// corners are rounder than Oak's other boxes: up to 22 points, as the app's
// message box has them. Oak draws its boxes as in any theme (a fill, the
// line's stroke, round corners); as the frame ends (`end_frame`, from
// `glass::Frame`) each rounded box in the theme's surfaces or with the
// line's stroke gets these edges in place of the flat stroke, so plugins'
// boxes and egui's widgets and windows are edged the same way.

/// The hairline's white along the top, the sides and the bottom.
const EDGE_TOP: f32 = 0.044;
const EDGE_SIDE: f32 = 0.030;
const EDGE_BOTTOM: f32 = 0.026;
/// The lighter line inside the top edge.
const TOP_LINE: f32 = 0.077;
/// The faint shadow in the pixel under the bottom edge (black).
const UNDER: f32 = 0.23;
/// The corners of a box at least `ROUNDER_H` tall are this much rounder,
/// up to `ROUNDEST`.
const ROUNDER: f32 = 1.5;
const ROUNDER_H: f32 = 40.0;
const ROUNDEST: f32 = 22.0;

thread_local! {
    /// This frame's lighting, while it is Antigravity.
    static LIGHT: std::cell::RefCell<Option<Light>> = const { std::cell::RefCell::new(None) };
}

/// What the frame's end lights: the theme's surfaces, each with how strongly
/// (a button is lit less than a card), and its line.
struct Light {
    surfaces: Vec<(Color32, f32)>,
    line: Color32,
}

/// A frame drawn in `t` begins: lit as it ends when `t` is this theme.
pub(crate) fn begin_frame(t: &Theme) {
    LIGHT.set(is(t).then(|| Light::of(t)));
}

/// The frame ends: its boxes get their edges.
pub(crate) fn end_frame(ctx: &egui::Context) {
    if let Some(l) = LIGHT.take() {
        l.finish(ctx);
    }
}

impl Light {
    fn of(t: &Theme) -> Light {
        let w = |f: f32| mix(t.bg, t.ink, f);
        Light {
            surfaces: vec![
                (t.panel, 1.0),
                (t.panel2, 1.0),
                (t.tool_bg, 1.0),
                // egui's buttons and fields (`theme::visuals`).
                (w(0.07), 1.0),
                (w(0.12), 1.0),
                (w(0.03), 1.0),
            ],
            line: t.line,
        }
    }

    fn strength(&self, c: Color32) -> Option<f32> {
        self.surfaces.iter().find(|(s, _)| *s == c).map(|(_, k)| *k)
    }

    fn finish(self, ctx: &egui::Context) {
        let ppp = ctx.pixels_per_point();
        let tfs: std::collections::HashMap<egui::LayerId, TSTransform> =
            ctx.memory(|m| m.to_global.iter().map(|(k, v)| (*k, *v)).collect());
        let mut layers: Vec<egui::LayerId> = ctx.memory(|m| m.layer_ids().collect());
        if !layers.contains(&egui::LayerId::background()) {
            layers.push(egui::LayerId::background());
        }
        ctx.graphics_mut(|g| {
            for layer in &layers {
                let Some(list) = g.get_mut(*layer) else { continue };
                let grid = Grid { ppp, tf: tfs.get(layer).copied() };
                for i in 0..list.next_idx().0 {
                    list.mutate_shape(egui::layers::ShapeIdx(i), |cs| {
                        if self.light(&mut cs.shape, grid) {
                            // A clip a fraction of a pixel into the box
                            // would dim its one-pixel edges.
                            let c = cs.clip_rect;
                            cs.clip_rect = Rect::from_min_max(
                                egui::pos2((c.left() * ppp).floor() / ppp, (c.top() * ppp).floor() / ppp),
                                egui::pos2((c.right() * ppp).ceil() / ppp, (c.bottom() * ppp).ceil() / ppp),
                            );
                        }
                    });
                }
            }
        });
    }

    /// Edges the boxes in `shape`; whether there were any.
    fn light(&self, shape: &mut egui::Shape, g: Grid) -> bool {
        match shape {
            egui::Shape::Vec(v) => {
                // Every one of them, not just up to the first.
                let mut any = false;
                for s in v {
                    any |= self.light(s, g);
                }
                any
            }
            egui::Shape::Rect(r) => match self.lit(r, g) {
                Some(parts) => {
                    *shape = egui::Shape::Vec(parts);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// `r` edged: its fill, rounder when it is tall enough, and the edges
    /// in place of the line's stroke (another stroke, a focus ring, stays,
    /// with the top line inside it). None when it isn't one of the theme's
    /// boxes.
    fn lit(&self, r: &egui::epaint::RectShape, g: Grid) -> Option<Vec<egui::Shape>> {
        let cr = r.corner_radius;
        if r.blur_width > 0.0
            || r.brush.is_some()
            || r.rect.width() < 14.0
            || r.rect.height() < 14.0
            || cr.nw.max(cr.ne).max(cr.sw).max(cr.se) < 4
        {
            return None;
        }
        let line = r.stroke.width > 0.0 && r.stroke.color == self.line;
        let surface = self.strength(r.fill);
        let k = match (line, surface) {
            (true, s) => s.unwrap_or(1.0),
            (false, Some(s)) => s,
            (false, None) => return None,
        };
        let bare = r.stroke.width <= 0.0 || r.stroke.color.a() == 0;
        let cr = rounder(cr, r.rect.height());
        let mut base = r.clone();
        base.corner_radius = cr;
        // The fill on the same whole pixels as the edges.
        base.rect = snap(r.rect, g);
        base.round_to_pixels = Some(false);
        if line {
            base.stroke = Stroke::NONE;
        }
        let ring = !line && !bare;
        let mut parts = vec![egui::Shape::Rect(base)];
        parts.push(edges(r.rect, cr, g, k, !ring));
        Some(parts)
    }
}

/// `cr`, rounder for a box `h` tall (a card, a bubble, the message box),
/// as the app's are.
fn rounder(cr: CornerRadius, h: f32) -> CornerRadius {
    if h < ROUNDER_H {
        return cr;
    }
    let most = ROUNDEST.min(h / 2.0);
    let r = |c: u8| {
        if c < 8 { c } else { (c as f32 * ROUNDER).min(most).max(c as f32).round() as u8 }
    };
    CornerRadius { nw: r(cr.nw), ne: r(cr.ne), sw: r(cr.sw), se: r(cr.se) }
}

/// Edges a box Oak paints itself with its own fill (a Home tile, the
/// dock): the hairline and the top line, `k` times as strong as a card's.
pub(crate) fn edge_light(p: &Painter, rect: Rect, radius: f32, k: f32) {
    let cr = CornerRadius::same(radius.round().clamp(0.0, 255.0) as u8);
    let g = Grid {
        ppp: p.ctx().pixels_per_point(),
        tf: p.ctx().memory(|m| m.to_global.get(&p.layer_id()).copied()),
    };
    p.add(edges(rect, cr, g, k, true));
}

/// The outline of `rect` with corners `cr`, `inset` inside it, clockwise
/// from the top left corner's arc.
fn outline(rect: Rect, cr: CornerRadius, inset: f32) -> Vec<Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI};
    let r = rect.shrink(inset);
    let max = (r.width().min(r.height()) / 2.0).max(0.0);
    let rad = |c: u8| (c as f32 - inset).clamp(0.0, max);
    let corners = [
        (rad(cr.nw), r.left_top(), vec2(1.0, 1.0), PI),
        (rad(cr.ne), r.right_top(), vec2(-1.0, 1.0), 1.5 * PI),
        (rad(cr.se), r.right_bottom(), vec2(-1.0, -1.0), 0.0),
        (rad(cr.sw), r.left_bottom(), vec2(1.0, -1.0), FRAC_PI_2),
    ];
    let mut pts = Vec::with_capacity(4 * 17);
    for (rr, corner, dir, a0) in corners {
        if rr < 0.5 {
            pts.push(corner);
            continue;
        }
        let c = corner + dir * rr;
        let n = ((rr * 0.6).ceil() as usize).clamp(3, 16);
        for i in 0..=n {
            let a = a0 + FRAC_PI_2 * i as f32 / n as f32;
            pts.push(c + rr * vec2(a.cos(), a.sin()));
        }
    }
    pts
}

/// Where a layer's shapes land on the screen: its pixels per point, and
/// the layer's transform (a layer moved or scaled).
#[derive(Clone, Copy)]
struct Grid {
    ppp: f32,
    tf: Option<TSTransform>,
}

impl Grid {
    /// A point on the screen, in the layer's points.
    fn px(&self) -> f32 {
        1.0 / (self.ppp * self.tf.map_or(1.0, |t| t.scaling)).max(0.5)
    }
}

/// `rect` (in its layer) on whole pixels of the screen.
fn snap(rect: Rect, g: Grid) -> Rect {
    let s = |v: f32| (v * g.ppp).round() / g.ppp;
    let on = g.tf.map_or(rect, |t| t * rect);
    let on = Rect::from_min_max(egui::pos2(s(on.left()), s(on.top())), egui::pos2(s(on.right()), s(on.bottom())));
    g.tf.map_or(on, |t| t.inverse() * on)
}

/// The hairline's white where the edge faces `up` (1 the top, 0 a side,
/// -1 the bottom).
fn edge_alpha(up: f32) -> f32 {
    EDGE_SIDE + (EDGE_TOP - EDGE_SIDE) * up.max(0.0) + (EDGE_BOTTOM - EDGE_SIDE) * (-up).max(0.0)
}

/// The top line's white where the edge faces `up`: all of it along the
/// top, fading round the corners, none down the sides.
fn top_alpha(up: f32) -> f32 {
    TOP_LINE * up.max(0.0).powi(2)
}

/// The shadow's black where the edge faces `up`: all of it along the
/// bottom, none from the sides up.
fn under_alpha(up: f32) -> f32 {
    UNDER * (-up).max(0.0).powi(2)
}

fn white(a: f32) -> Color32 {
    Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// The edges: the hairline in the box's outermost pixel and the top line
/// in the pixel inside it (when `top`), each a crisp pixel wide.
fn edges(rect: Rect, cr: CornerRadius, g: Grid, k: f32, top: bool) -> egui::Shape {
    let px = g.px();
    // On whole pixels, so each line is one pixel.
    let rect = snap(rect, g);
    let pts = outline(rect, cr, 0.0);
    let n = pts.len();
    // Each point's outward normal.
    let mut normals = Vec::with_capacity(n);
    let mut last = vec2(0.0, -1.0);
    for i in 0..n {
        let tan = pts[(i + 1) % n] - pts[(i + n - 1) % n];
        if tan.length() > 1e-4 {
            let d = tan / tan.length();
            last = vec2(d.y, -d.x);
        }
        normals.push(last);
    }
    let mut m = Mesh::default();
    // A band from `d0` to `d1` pixels inside the edge (outside it below
    // 0), soft by a fifth of a pixel at each side, in `color` where the
    // edge faces up by so much.
    let mut band = |d0: f32, d1: f32, color: &dyn Fn(f32) -> Color32| {
        let base = m.vertices.len() as u32;
        for (p, nrm) in pts.iter().zip(&normals) {
            let col = color(-nrm.y);
            for (d, solid) in [(d0 - 0.2, false), (d0 + 0.2, true), (d1 - 0.2, true), (d1 + 0.2, false)] {
                m.colored_vertex(*p - *nrm * (d * px), if solid { col } else { Color32::TRANSPARENT });
            }
        }
        for i in 0..n as u32 {
            let (a, b) = (base + 4 * i, base + 4 * ((i + 1) % n as u32));
            for j in 0..3 {
                m.add_triangle(a + j, a + j + 1, b + j);
                m.add_triangle(a + j + 1, b + j, b + j + 1);
            }
        }
    };
    band(0.0, 1.0, &|up| white(edge_alpha(up) * k));
    if top {
        band(1.0, 2.0, &|up| white(top_alpha(up) * k));
    }
    band(-1.0, 0.0, &|up| Color32::from_black_alpha((under_alpha(up) * k.min(1.0) * 255.0).round() as u8));
    egui::Shape::mesh(m)
}

/// The page's pane in `area` (what is left of the window under the title
/// bar and right of the sidebar): inset from the right and bottom edges.
pub(crate) fn pane(area: Rect) -> Rect {
    Rect::from_min_max(area.min, area.max - vec2(PANE_MARGIN, PANE_MARGIN))
}

/// Cuts the pane's corners round, over whatever the page painted there:
/// each corner outside the arc in the shell's color (with a soft edge), and
/// the pane's border. On a layer of its own above the page.
pub(crate) fn mask_pane(ctx: &egui::Context, t: &Theme, pane: Rect) {
    let p = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::unique("gravity-pane-mask"),
    ));
    let r = PANE_R;
    let sh = shell();
    // (corner, centre of its arc, angle the arc starts at)
    let corners = [
        (
            pane.left_top(),
            pane.left_top() + vec2(r, r),
            std::f32::consts::PI,
        ),
        (
            pane.right_top(),
            pane.right_top() + vec2(-r, r),
            1.5 * std::f32::consts::PI,
        ),
        (pane.right_bottom(), pane.right_bottom() + vec2(-r, -r), 0.0),
        (
            pane.left_bottom(),
            pane.left_bottom() + vec2(r, -r),
            0.5 * std::f32::consts::PI,
        ),
    ];
    for (corner, centre, a0) in corners {
        let mut m = Mesh::default();
        m.colored_vertex(corner, sh);
        let n = 12u32;
        for i in 0..=n {
            // Angles measured from +x, clockwise in screen terms
            // (y down): the quarter facing the corner.
            let a = a0 + (i as f32 / n as f32) * std::f32::consts::FRAC_PI_2;
            let d = vec2(a.cos(), a.sin());
            m.colored_vertex(centre + d * (r + 0.5), sh);
            m.colored_vertex(centre + d * (r - 0.6), Color32::TRANSPARENT);
        }
        for i in 0..n {
            let (a, b) = (1 + 2 * i, 3 + 2 * i);
            m.add_triangle(0, a, b);
            m.add_triangle(a, a + 1, b);
            m.add_triangle(a + 1, b, b + 1);
        }
        p.add(m);
    }
    p.rect_stroke(pane, r, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The theme is told apart from the others by its colors, and only it
    /// rounds corners.
    #[test]
    fn only_this_theme_rounds() {
        let t = theme();
        assert!(is(&t));
        assert!(!is(&Theme::oak()) && !is(&crate::glass::theme()));
        assert_eq!(round(&t, 8.0), 8.0);
        assert_eq!(round(&Theme::oak(), 8.0), 0.0);
        assert_eq!(radius(&crate::glass::theme(), 8.0), CornerRadius::ZERO);
    }

    #[test]
    fn switching_themes_restores_widget_style() {
        let ctx = egui::Context::default();
        let t = crate::theme::theme(ID);
        assert_eq!(t, theme());
        crate::theme::apply_theme(&ctx, &t);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark)
                .visuals
                .widgets
                .inactive
                .corner_radius,
            CornerRadius::same(8)
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).spacing.button_padding,
            vec2(12.0, 6.0)
        );
        crate::theme::apply_theme(&ctx, &Theme::oak());
        assert_eq!(
            ctx.style_of(egui::Theme::Dark)
                .visuals
                .widgets
                .inactive
                .corner_radius,
            CornerRadius::ZERO
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).spacing.button_padding,
            vec2(10.0, 5.0)
        );
    }

    /// A rounded card is edged (its fill, rounder, and the edges, the
    /// line's stroke gone), a square box or a color not the theme's is
    /// left alone, and the edges are as measured from the app.
    #[test]
    fn rounded_boxes_are_lit() {
        let t = theme();
        let l = Light::of(&t);
        let card = egui::epaint::RectShape::new(
            Rect::from_min_size(Pos2::ZERO, vec2(200.0, 80.0)),
            CornerRadius::same(12),
            t.panel,
            Stroke::new(1.0, t.line),
            egui::StrokeKind::Inside,
        );
        let g = Grid { ppp: 1.0, tf: None };
        let parts = l.lit(&card, g).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], egui::Shape::Rect(r) if r.stroke == Stroke::NONE && r.corner_radius == CornerRadius::same(18)));
        let mut square = card.clone();
        square.corner_radius = CornerRadius::ZERO;
        assert!(l.lit(&square, g).is_none());
        let mut other = card.clone();
        other.fill = hex(0x123456);
        other.stroke = Stroke::NONE;
        assert!(l.lit(&other, g).is_none());
        // A small button keeps its corners.
        assert_eq!(rounder(CornerRadius::same(8), 26.0), CornerRadius::same(8));
        assert!(edge_alpha(1.0) > edge_alpha(0.0) && edge_alpha(0.0) > edge_alpha(-1.0));
        assert_eq!(top_alpha(0.0), 0.0);
        assert_eq!(top_alpha(1.0), TOP_LINE);
        assert_eq!(under_alpha(0.0), 0.0);
        assert_eq!(under_alpha(-1.0), UNDER);
        assert_eq!(outline(card.rect, card.corner_radius, 0.0).len(), 4 * 9);
    }

    /// The gradient runs from the logo's blue to its red.
    #[test]
    fn brand_gradient_ends() {
        assert_eq!(brand_at(0.0), hex(BRAND[0]));
        assert_eq!(brand_at(1.0), hex(BRAND[5]));
        assert_eq!(brand_at(-1.0), hex(BRAND[0]));
    }
}
