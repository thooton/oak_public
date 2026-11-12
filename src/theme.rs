//! Themes (colors only: `oak_sdk::theme`; Oak's own is built in, plugins
//! add others) and egui's style from them.

use super::*;
pub(crate) use oak_sdk::theme::{Theme, hex, mix};

/// The id of Oak's own theme (jet black, white, oak green).
pub(crate) const OAK: &str = "oak";

/// The themes Settings offers: Oak's own, then the plugins', as (id, name).
pub(crate) fn themes() -> Vec<(String, String)> {
    let mut list = vec![(OAK.to_string(), "Oak".to_string())];
    list.extend(crate::plugins::parts().themes.iter().map(|d| (d.id(), d.name())).filter(|(id, _)| id != OAK));
    list
}

/// The theme with this id: Oak's own for its id, an unknown one, or one
/// whose plugin isn't loaded (the setting keeps the id, for when it is).
pub(crate) fn theme(id: &str) -> Theme {
    crate::plugins::parts().themes.iter().find(|d| id != OAK && d.id() == id).map_or_else(Theme::oak, |d| d.theme())
}

pub(crate) fn apply_theme(ctx: &egui::Context, t: &Theme) {
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
    ctx.set_visuals(v);
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
