//! Small widgets and painted icons the window's parts share: captions,
//! clipped text, icon buttons, a page's close button, the adding boxes'
//! fields, bordered boxes, Enter in text boxes, drop-downs' headers and
//! what is open, a page's centered note, the fades at a strip's ends, and
//! the x, pencil, pause and drop-down signs.

use crate::egui::{self, Align, Align2, Button, Color32, FontId, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, Rect, RichText, Sense, Stroke, TextEdit, Ui, UiBuilder, vec2};
use crate::theme::Theme;

/// Lays `f` out in a centered column at most `w` wide.
pub fn column<R>(ui: &mut Ui, w: f32, f: impl FnOnce(&mut Ui) -> R) -> R {
    let avail = ui.available_rect_before_wrap();
    let w = w.min(avail.width() - 32.0).max(100.0);
    let x = avail.center().x - w / 2.0;
    let rect = Rect::from_min_max(egui::pos2(x, avail.top()), egui::pos2(x + w, avail.bottom()));
    ui.scope_builder(UiBuilder::new().max_rect(rect), f).inner
}

/// A small caption over a field or a section ("Theme", "Model").
pub fn caption(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    ui.label(RichText::new(text).small().color(color))
}

/// How wide `s` is on one line in `font`.
pub fn text_width(ui: &Ui, s: &str, font: &FontId) -> f32 {
    ui.fonts_mut(|f| f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x)
}

/// The first line of `s`, cut with "…" to fit `w` points in `font`.
pub fn clip_width(ui: &Ui, s: &str, font: &FontId, w: f32) -> String {
    let s = s.lines().next().unwrap_or("");
    let width = |t: &str| text_width(ui, t, font);
    if width(s) <= w {
        return s.to_string();
    }
    // The longest prefix that fits, with the ellipsis, by bisection.
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let fit = |n: usize| width(&format!("{}…", chars[..n].iter().collect::<String>())) <= w;
        if fit(mid) { lo = mid } else { hi = mid - 1 }
    }
    format!("{}…", chars[..lo].iter().collect::<String>())
}

/// A "v" (or, `up`, a "^") with no stem: a drop-down's arrow.
pub fn chevron_v(p: &egui::Painter, at: egui::Pos2, up: bool, color: Color32) {
    let (s, h) = (3.5, if up { -1.75 } else { 1.75 });
    let pts = [at + vec2(-s, -h), at + vec2(0.0, h), at + vec2(s, -h)];
    p.line_segment([pts[0], pts[1]], Stroke::new(1.4, color));
    p.line_segment([pts[1], pts[2]], Stroke::new(1.4, color));
}

/// A ">" that turns to point down (`chevron_v`'s "v") when its row is open.
pub fn chevron(p: &egui::Painter, at: egui::Pos2, open: bool, color: Color32) {
    if open {
        return chevron_v(p, at, false, color);
    }
    let (s, tip) = (3.5, at + vec2(1.75, 0.0));
    p.line_segment([at + vec2(-s * 0.5, -s), tip], Stroke::new(1.4, color));
    p.line_segment([tip, at + vec2(-s * 0.5, s)], Stroke::new(1.4, color));
}

/// A conversation's dot: a ring when it is `paused`, else solid (working).
pub fn state_dot(p: &egui::Painter, t: &Theme, at: egui::Pos2, paused: bool) {
    if paused {
        p.circle_stroke(at, 3.0, Stroke::new(1.2, t.ink3));
    } else {
        p.circle_filled(at, 3.0, t.accent);
    }
}

/// The pause sign: two bars in `r`.
pub fn paint_pause(p: &egui::Painter, r: Rect, color: Color32) {
    let (w, h) = (r.width() * 0.22, r.height() * 0.7);
    let top = r.center().y - h / 2.0;
    for x in [r.center().x - w * 1.4, r.center().x + w * 0.4] {
        p.rect_filled(Rect::from_min_size(egui::pos2(x, top), vec2(w, h)), 1.0, color);
    }
}

/// A small clickable square with an icon painted in it.
pub fn icon_button(ui: &mut Ui, t: &Theme, rect: Rect, id: Id, tip: &str, draw: fn(&egui::Painter, Rect, Color32)) -> bool {
    let resp = ui.interact(rect, id, Sense::click());
    let color = if resp.hovered() { t.ink } else { t.ink2 };
    ui.painter().rect_filled(rect, 0.0, if resp.hovered() { t.panel2 } else { t.bg });
    ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
    draw(ui.painter(), rect, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
}

/// `c` fading out across `r`: from solid at its left edge (`solid_left`)
/// or at its right.
pub fn fade(p: &egui::Painter, r: Rect, c: Color32, solid_left: bool) {
    let (a, b) = if solid_left { (c, Color32::TRANSPARENT) } else { (Color32::TRANSPARENT, c) };
    let mut m = egui::Mesh::default();
    m.colored_vertex(r.left_top(), a);
    m.colored_vertex(r.right_top(), b);
    m.colored_vertex(r.right_bottom(), b);
    m.colored_vertex(r.left_bottom(), a);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(m);
}

pub fn draw_x(p: &egui::Painter, r: Rect, c: Color32) {
    let (m, s) = (r.center(), r.width() * 0.2);
    let stroke = Stroke::new(1.5, c);
    p.line_segment([m + vec2(-s, -s), m + vec2(s, s)], stroke);
    p.line_segment([m + vec2(-s, s), m + vec2(s, -s)], stroke);
}

/// A pencil pointing down-left: tip, body and the band by the eraser.
pub fn draw_pencil(p: &egui::Painter, r: Rect, c: Color32) {
    let (m, s) = (r.center(), r.width() * 0.3);
    let u = vec2(1.0, -1.0).normalized() * s; // along the pencil, towards the eraser
    let n = vec2(1.0, 1.0).normalized() * s * 0.3; // across it
    let tip = m - u;
    let (b0, b1) = (m - u * 0.45, m + u);
    p.add(egui::Shape::closed_line(vec![tip, b0 + n, b1 + n, b1 - n, b0 - n], Stroke::new(1.3, c)));
    let band = m + u * 0.6;
    p.line_segment([band + n, band - n], Stroke::new(1.3, c));
}

/// An adding box's heading, with a link that opens the sign-in page at
/// `url` again.
pub fn sign_in_heading(ui: &mut Ui, t: &Theme, title: &str, url: &str) {
    ui.horizontal(|ui| {
        caption(ui, title, t.ink2);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let again = ui.add(egui::Label::new(RichText::new("open page").small().color(t.accent)).sense(Sense::click()));
            if again.on_hover_text("Open the sign-in page again").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                let _ = open::that(url);
            }
        });
    });
}

/// A spinner and what is under way.
pub fn working(ui: &mut Ui, t: &Theme, what: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(RichText::new(what).color(t.ink3));
    });
}

/// A short note centered in `rect` (an empty list, a page that needs a
/// connection).
pub fn center_note(ui: &Ui, t: &Theme, rect: Rect, text: &str) {
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(12.5), t.ink3);
}

/// "Loading…", or what kept it from loading.
pub fn loading(ui: &mut Ui, t: &Theme, error: Option<&str>) {
    match error {
        Some(e) => error_note(ui, t, e),
        None => working(ui, t, "Loading…"),
    }
}

/// What went wrong, small and wrapped.
pub fn error_note(ui: &mut Ui, t: &Theme, e: &str) {
    ui.add(egui::Label::new(RichText::new(e).small().color(t.error)).wrap());
}

/// An adding box's field (`edit`, made small), what went wrong (or `note`
/// when nothing did), and Add and Cancel. Returns (Add pressed or Enter,
/// Cancel pressed).
pub fn add_field(ui: &mut Ui, t: &Theme, edit: TextEdit, secret: Option<&str>, error: Option<&String>, note: impl FnOnce(&mut Ui)) -> (bool, bool) {
    let edit = edit.font(egui::TextStyle::Small).margin(Margin::symmetric(4, 2));
    let edit = match secret {
        Some(salt) => secret_edit(ui, t, salt, edit, true, None),
        None => crate::text_menu::add(ui, edit.desired_width(f32::INFINITY)),
    };
    let enter = left_with(ui, &edit, Key::Enter);
    match error {
        Some(e) => drop(ui.add(egui::Label::new(RichText::new(e).small().color(t.error)).truncate()).on_hover_text(e)),
        None => note(ui),
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().button_padding = vec2(8.0, 1.0);
        let add = ui.button(RichText::new("Add").small()).clicked() || enter;
        (add, ui.button(RichText::new("Cancel").small()).clicked())
    })
    .inner
}

/// Where `secret_edit` keeps whether the secret named `salt` is shown.
#[allow(deprecated)]
pub fn secret_key(salt: &str) -> Id {
    Id::new(("secret-shown", salt))
}

/// A text box for a secret: masked until its Show button (Hide when shown)
/// is clicked. `salt` names the field for the remembered state, which is
/// per session of the window only and starts masked. `small` sizes the button
/// to match a small-font box. The box keeps the usual padding, so it is as
/// tall as the boxes around it; only the button takes the small one.
pub fn secret_edit(ui: &mut Ui, t: &Theme, salt: &str, edit: TextEdit, small: bool, width: Option<f32>) -> egui::Response {
    let key = secret_key(salt);
    let shown = ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(false);
    let word = if shown { "Hide" } else { "Show" };
    let total = width.unwrap_or(f32::INFINITY).min(ui.available_width());
    let resp = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let btn_w = if small { 38.0 } else { 46.0 };
        let edit = crate::text_menu::add_secret(ui, edit.desired_width((total - btn_w - 4.0).max(40.0)), !shown).clone();
        ui.spacing_mut().button_padding = vec2(6.0, 1.0);
        let label = RichText::new(word).color(t.ink2);
        let label = if small { label.small() } else { label };
        let btn = ui.add_sized(vec2(btn_w, edit.rect.height()), egui::Button::new(label));
        if btn.on_hover_text(format!("{word} the secret")).clicked() {
            ui.data_mut(|d| d.insert_temp(key, !shown));
        }
        edit
    });
    resp.inner
}

/// An on/off switch `size` big, its knob at the end it is set to.
pub fn switch(ui: &mut Ui, t: &Theme, on: bool, size: egui::Vec2) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 0.0, if on { t.accent } else { t.panel2 });
    p.rect_stroke(rect, 0.0, Stroke::new(1.0, if resp.hovered() { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
    let r = size.y / 2.0;
    let knob = Rect::from_center_size(egui::pos2(if on { rect.right() - r } else { rect.left() + r }, rect.center().y), egui::Vec2::splat(size.y - 6.0));
    p.rect_filled(knob, 0.0, if on { t.bg } else { t.ink3 });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A page's card (Settings, Usage) on the page color: at most `max_w`
/// wide, 24 points clear of the window's edges above and below, in a
/// border (over `fill`), with a heading over a line and the page's X at
/// its right. Returns the card, its heading, and whether the X was
/// clicked or Escape pressed.
pub fn page_card(ui: &mut Ui, t: &Theme, max_w: f32, fill: Option<Color32>, close_id: &str) -> (Rect, Rect, bool) {
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, t.bg);
    let w = (rect.width() - 32.0).min(max_w);
    let card = Rect::from_min_max(egui::pos2(rect.center().x - w / 2.0, rect.top() + 24.0), egui::pos2(rect.center().x + w / 2.0, rect.bottom() - 24.0));
    let head = Rect::from_min_max(card.min, egui::pos2(card.right(), card.top() + crate::ui::HOME_HEAD));
    let p = ui.painter();
    if let Some(fill) = fill {
        p.rect_filled(card, 0.0, fill);
    }
    p.rect_stroke(card, 0.0, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
    p.hline(card.x_range(), head.bottom() - 0.5, Stroke::new(1.0, t.line));
    let x = Rect::from_center_size(egui::pos2(head.right() - head.height() / 2.0, head.center().y), vec2(24.0, 24.0));
    let close = close_button(ui, t, x, close_id);
    let escape = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    (card, head, close || escape)
}

/// A page's X in `rect` (it lights up under the pointer). True when clicked.
pub fn close_button(ui: &mut Ui, t: &Theme, rect: Rect, id: &str) -> bool {
    let resp = ui.interact(rect, Id::unique(id), Sense::click());
    let lit = resp.hovered();
    if lit {
        ui.painter().rect_filled(rect, 0.0, t.panel2);
    }
    draw_x(ui.painter(), rect, if lit { t.ink } else { t.ink3 });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Close (Esc)").clicked()
}

/// Text box `edit` left with `key` (Enter, Escape).
pub fn left_with(ui: &Ui, edit: &egui::Response, key: Key) -> bool {
    edit.lost_focus() && ui.input(|i| i.key_pressed(key))
}

/// A text box Enter sends from (`enter_pressed`), Shift+Enter making a
/// newline.
pub fn sending_edit(text: &mut String, id: Id) -> TextEdit<'_> {
    TextEdit::multiline(text).id(id).return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
}

/// Enter pressed in text box `id`, which has the keyboard (taken, so the
/// box doesn't get a newline); Shift+Enter is left to it as a newline
/// (consume_key alone would match Shift+Enter too).
pub fn enter_pressed(ui: &Ui, id: Id) -> bool {
    ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter))
}

/// A dialog over the dimmed window.
pub fn dialog(id: &str, t: &Theme) -> egui::Modal {
    egui::Modal::new(egui::Id::unique(id))
        .backdrop_color(Color32::from_black_alpha(150))
        .frame(egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).inner_margin(Margin::same(18)))
}

/// A dialog asking before something that can't be undone: `title`, what
/// `body` adds, what went wrong (`error`), then `action` (in the error
/// colour) and Cancel. While `running`, the action is disabled beside a
/// spinner and only Cancel closes it. Returns (the action clicked, closed:
/// Cancel, Esc or a click outside).
#[allow(clippy::too_many_arguments)]
pub fn confirm_dialog(ctx: &egui::Context, t: &Theme, id: &str, title: &str, body: impl FnOnce(&mut Ui), error: Option<&str>, action: &str, running: bool) -> (bool, bool) {
    let (mut act, mut cancel) = (false, false);
    let modal = dialog(id, t).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.label(RichText::new(title).size(16.0).color(t.ink));
        body(ui);
        if let Some(e) = error {
            error_note(ui, t, e);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            act = ui.add_enabled(!running, Button::new(RichText::new(action).color(t.error))).clicked();
            cancel = ui.button("Cancel").clicked();
            if running {
                ui.spinner();
            }
        });
    });
    (act, cancel || (modal.should_close() && !running))
}

/// A box as wide as the column, with a thin border: `fill` behind what
/// `f` adds, `margin` inside.
pub fn inset<R>(ui: &mut Ui, t: &Theme, fill: Color32, margin: i8, f: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, t.line))
        .inner_margin(Margin::same(margin))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            f(ui)
        })
        .inner
}

/// A drop-down's header row, `h` tall: a chevron, its title and `count`,
/// `right` at the right. A click opens or closes it (unless not `enabled`:
/// then it stays closed). Returns whether it is open, and the row's
/// response.
#[allow(clippy::too_many_arguments)]
pub fn drop_head(ui: &mut Ui, t: &Theme, id: Id, h: f32, title: &str, count: &str, right: (&str, Color32), enabled: bool) -> (bool, egui::Response) {
    let mut open = is_open(ui, id, false);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), if enabled { Sense::click() } else { Sense::hover() });
    let lit = enabled && resp.hovered();
    if enabled && resp.clicked() {
        open = !open;
        set_open(ui, id, open);
    }
    let open = open && enabled;
    let p = ui.painter();
    let y = r.center().y;
    chevron(p, egui::pos2(r.left() + 4.0, y), open, if lit { t.ink } else if enabled { t.ink3 } else { t.ink4 });
    let small = egui::TextStyle::Small.resolve(ui.style());
    let g = p.text(egui::pos2(r.left() + 16.0, y), Align2::LEFT_CENTER, title, FontId::proportional(13.0), if enabled { t.ink } else { t.ink3 });
    p.text(egui::pos2(g.right() + 8.0, y), Align2::LEFT_CENTER, count, small.clone(), t.ink3);
    p.text(egui::pos2(r.right(), y), Align2::RIGHT_CENTER, right.0, small, right.1);
    if lit {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    (open, resp)
}

/// Whether the thing `id` is open (`default` until it is toggled).
pub fn is_open(ui: &Ui, id: Id, default: bool) -> bool {
    ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(default)
}

pub fn set_open(ui: &Ui, id: Id, open: bool) {
    ui.data_mut(|d| d.insert_temp(id, open));
}
