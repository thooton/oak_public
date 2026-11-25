//! Themes (colors: `oak_sdk::theme`; Oak's own, Liquid Glass and
//! Antigravity are built in, plugins add others) and egui's style from
//! them. Antigravity (`gravity.rs`) also rounds egui's corners and pads its
//! buttons more.

use super::*;
pub(crate) use oak_sdk::theme::{Theme, hex, mix};

/// The id of Oak's own theme (jet black, white, oak green).
pub(crate) const OAK: &str = "oak";

/// The id of Liquid Glass (`glass`: Antigravity's colors, as glass).
pub(crate) const GLASS: &str = crate::glass::ID;

/// The id of Antigravity (`antigravity`: the Hub's look, its shapes and
/// layout too).
pub(crate) const GRAVITY: &str = crate::gravity::ID;

/// The themes Settings offers: Oak's own, Liquid Glass, Antigravity, then
/// the plugins', as (id, name).
pub(crate) fn themes() -> Vec<(String, String)> {
    let mut list = vec![
        (OAK.to_string(), "Oak".to_string()),
        (GLASS.to_string(), crate::glass::NAME.to_string()),
        (GRAVITY.to_string(), crate::gravity::NAME.to_string()),
    ];
    list.extend(crate::plugins::parts().themes.iter().map(|d| (d.id(), d.name())).filter(|(id, _)| id != OAK && id != GLASS && id != GRAVITY));
    list
}

/// The theme with this id: a built-in one for its id; Oak's own for an
/// unknown one, or one whose plugin isn't loaded (the setting keeps the id,
/// for when it is).
pub(crate) fn theme(id: &str) -> Theme {
    if id == GLASS {
        return crate::glass::theme();
    }
    if id == GRAVITY {
        return crate::gravity::theme();
    }
    crate::plugins::parts().themes.iter().find(|d| id != OAK && d.id() == id).map_or_else(Theme::oak, |d| d.theme())
}

pub(crate) fn apply_theme(ctx: &egui::Context, t: &Theme) {
    ctx.set_visuals(visuals(t));
    // Antigravity's buttons are roomier, as the Hub's are.
    let round = crate::gravity::is(t);
    ctx.all_styles_mut(|s| {
        s.spacing.button_padding = if round { vec2(12.0, 6.0) } else { vec2(10.0, 5.0) };
        s.spacing.menu_margin = if round { Margin::same(8) } else { Margin::same(6) };
    });
}

/// egui's colors from `t` (also a look's scope's: `view::look_scope`).
pub(crate) fn visuals(t: &Theme) -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.panel_fill = t.bg;
    v.window_fill = t.panel;
    v.extreme_bg_color = t.panel;
    v.faint_bg_color = t.panel;
    v.code_bg_color = t.panel2;
    v.override_text_color = Some(t.ink);
    v.hyperlink_color = t.accent;
    v.window_stroke = Stroke::new(1.0, t.line);
    v.selection.bg_fill = t.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, t.ink);
    v.text_cursor.stroke = Stroke::new(2.0, t.accent);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    for (w, fill, stroke) in [
        (&mut v.widgets.noninteractive, t.bg, t.line),
        (&mut v.widgets.inactive, t.panel, t.line),
        (&mut v.widgets.hovered, t.panel2, t.ink3),
        (&mut v.widgets.active, t.panel2, t.accent),
        (&mut v.widgets.open, t.panel2, t.accent),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, t.ink);
        w.corner_radius = CornerRadius::ZERO;
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.ink);
    // In Liquid Glass a selected item is the indicator lens: its fill is
    // the mark that becomes it.
    // And glass casts no shadow (the package's iOS 27 materials have none):
    // under a popup it would be what the popup frosts.
    if crate::glass::is(t) {
        v.selection.bg_fill = crate::glass::SELECT;
        v.window_shadow = egui::Shadow::NONE;
        v.popup_shadow = egui::Shadow::NONE;
    }
    // Antigravity's corners are round, its floating surfaces cast a soft
    // shadow, and a field or button sits a step above the page.
    if crate::gravity::is(t) {
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = egui::Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(110) };
        v.selection.bg_fill = t.accent.gamma_multiply(0.30);
        v.extreme_bg_color = mix(t.bg, t.ink, 0.03);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.inactive.bg_fill = mix(t.bg, t.ink, 0.07);
        v.widgets.inactive.weak_bg_fill = v.widgets.inactive.bg_fill;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);
        v.widgets.hovered.bg_fill = mix(t.bg, t.ink, 0.12);
        v.widgets.hovered.weak_bg_fill = v.widgets.hovered.bg_fill;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, t.line);
    }
    v
}

pub(crate) fn setup_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        use egui::TextStyle::*;
        // IBM Plex Sans throughout; the Monospace style is for code.
        for (style, size) in [(Small, 11.5), (Body, 14.0), (Button, 13.5), (Heading, 21.0)] {
            s.text_styles.insert(style, FontId::proportional(size));
        }
        s.text_styles.insert(Monospace, markdown::code_font(13.0));
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
    });
    // The wheel scrolls as far per notch as a browser does.
    ctx.options_mut(|o| o.input_options.line_scroll_speed = wheel_notch());
}
