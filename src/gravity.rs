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

    /// The gradient runs from the logo's blue to its red.
    #[test]
    fn brand_gradient_ends() {
        assert_eq!(brand_at(0.0), hex(BRAND[0]));
        assert_eq!(brand_at(1.0), hex(BRAND[5]));
        assert_eq!(brand_at(-1.0), hex(BRAND[0]));
    }
}
