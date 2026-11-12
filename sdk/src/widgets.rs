//! Small widgets and painted icons Oak's pages are made of: captions,
//! clipped text, the adding boxes' fields, bordered boxes, Enter in text
//! boxes, drop-downs' headers and what is open, a page's centered note,
//! dialogs, and the x, pause and drop-down signs. Oak draws most of them
//! (`abi`'s `oak_` functions); the theme arguments are kept for the code
//! that passes them, and Oak uses the theme in use.

use crate::abi::*;
use crate::egui::{self, Color32, FontId, Id, Key, Modifiers, Rect, Response, RichText, Stroke, TextEdit, Ui, UiBuilder, color_in, rect_in, vec2, with_body};
use crate::rt::{api, s};
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
pub fn caption(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let mut out = OakResponse::default();
    unsafe { (api().oak_caption)(ui.as_raw(), s(text), color_in(color), &mut out) };
    Response::of(out)
}

/// How wide `s` is on one line in `font`.
pub fn text_width(ui: &Ui, s: &str, font: &FontId) -> f32 {
    egui::text_size(ui, &RichText::new(s).font(font.clone()), 0.0).x
}

/// The first line of `s`, cut with "…" to fit `w` points in `font`.
pub fn clip_width(ui: &Ui, s: &str, font: &FontId, w: f32) -> String {
    egui::text_clip(ui, &RichText::new(s.lines().next().unwrap_or("")).font(font.clone()), w)
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
pub fn state_dot(p: &egui::Painter, _t: &Theme, at: egui::Pos2, paused: bool) {
    unsafe { (api().oak_state_dot)(p.raw(), at.x, at.y, paused) };
}

/// The pause sign: two bars in `r`.
pub fn paint_pause(p: &egui::Painter, r: Rect, color: Color32) {
    let (w, h) = (r.width() * 0.22, r.height() * 0.7);
    let top = r.center().y - h / 2.0;
    for x in [r.center().x - w * 1.4, r.center().x + w * 0.4] {
        p.rect_filled(Rect::from_min_size(egui::pos2(x, top), vec2(w, h)), 1.0, color);
    }
}

pub fn draw_x(p: &egui::Painter, r: Rect, c: Color32) {
    let (m, s) = (r.center(), r.width() * 0.2);
    let stroke = Stroke::new(1.5, c);
    p.line_segment([m + vec2(-s, -s), m + vec2(s, s)], stroke);
    p.line_segment([m + vec2(-s, s), m + vec2(s, -s)], stroke);
}

/// A spinner and what is under way.
pub fn working(ui: &mut Ui, _t: &Theme, what: &str) {
    unsafe { (api().oak_working)(ui.as_raw(), s(what)) };
}

/// A short note centered in `rect` (an empty list, a page that needs a
/// connection).
pub fn center_note(ui: &Ui, _t: &Theme, rect: Rect, text: &str) {
    unsafe { (api().oak_center_note)(ui.painter().raw(), &rect_in(rect), s(text)) };
}

/// "Loading…", or what kept it from loading.
pub fn loading(ui: &mut Ui, _t: &Theme, error: Option<&str>) {
    unsafe { (api().oak_loading)(ui.as_raw(), s(error.unwrap_or(""))) };
}

/// What went wrong, small and wrapped.
pub fn error_note(ui: &mut Ui, _t: &Theme, e: &str) {
    unsafe { (api().oak_error_note)(ui.as_raw(), s(e)) };
}

/// A text box for a secret: masked until its Show button (Hide when shown)
/// is clicked. `salt` names the field for the remembered state, which is
/// per session of the window only and starts masked. `small` sizes the
/// button to match a small-font box.
pub fn secret_edit(ui: &mut Ui, _t: &Theme, salt: &str, edit: TextEdit, small: bool, width: Option<f32>) -> Response {
    let raw = ui.as_raw();
    let edit = edit.flag(OAK_EDIT_SMALL, small);
    let edit = match width {
        Some(w) => edit.desired_width(w),
        None => edit,
    };
    edit.show_with(ui, |edit, text, sink, user, out| unsafe { (api().oak_secret_edit)(raw, s(salt), edit, text, sink, user, out) })
}

/// An on/off switch `size` big, its knob at the end it is set to.
pub fn switch(ui: &mut Ui, _t: &Theme, on: bool, size: egui::Vec2) -> Response {
    let mut out = OakResponse::default();
    unsafe { (api().oak_switch)(ui.as_raw(), on, size.x, size.y, &mut out) };
    Response::of(out)
}

/// Text box `edit` left with `key` (Enter, Escape).
pub fn left_with(ui: &Ui, edit: &Response, key: Key) -> bool {
    edit.lost_focus() && ui.input(|i| i.key_pressed(key))
}

/// A text box Enter sends from (`enter_pressed`), Shift+Enter making a
/// newline.
pub fn sending_edit(text: &mut String, id: Id) -> TextEdit<'_> {
    TextEdit::multiline(text).id(id).enter_sends()
}

/// Enter pressed in text box `id`, which has the keyboard (taken, so the
/// box doesn't get a newline); Shift+Enter is left to it as a newline.
pub fn enter_pressed(ui: &Ui, id: Id) -> bool {
    ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter))
}

/// A dialog over the dimmed window (`dialog`).
pub struct Dialog {
    id: String,
}

/// What a dialog's closure returned, and whether the dialog should close.
pub struct Modal<R> {
    pub inner: R,
    close: bool,
}

impl<R> Modal<R> {
    /// Escape was pressed, or a click landed outside it.
    pub fn should_close(&self) -> bool {
        self.close
    }
}

impl Dialog {
    pub fn show<R>(self, _ctx: &egui::Context, f: impl FnOnce(&mut Ui) -> R) -> Modal<R> {
        let (inner, flags) = with_body(f, |body, user| unsafe { (api().oak_dialog)(s(&self.id), body, user) });
        Modal { inner: inner.expect("Oak didn't draw the dialog"), close: flags & OAK_CANCELLED != 0 }
    }
}

/// A dialog over the dimmed window.
pub fn dialog(id: &str, _t: &Theme) -> Dialog {
    Dialog { id: id.to_string() }
}

/// A dialog asking before something that can't be undone: `title`, what
/// `body` adds, what went wrong (`error`), then `action` (in the error
/// colour) and Cancel. While `running`, the action is disabled beside a
/// spinner and only Cancel closes it. Returns (the action clicked, closed:
/// Cancel, Esc or a click outside).
#[allow(clippy::too_many_arguments)]
pub fn confirm_dialog(_ctx: &egui::Context, _t: &Theme, id: &str, title: &str, body: impl FnOnce(&mut Ui), error: Option<&str>, action: &str, running: bool) -> (bool, bool) {
    let (_, flags) = with_body(body, |body, user| unsafe { (api().oak_confirm_dialog)(s(id), s(title), Some(body), user, s(error.unwrap_or("")), s(action), running) });
    (flags & OAK_CONFIRMED != 0, flags & OAK_CANCELLED != 0)
}

/// A box as wide as the column, with a thin border: `fill` behind what
/// `f` adds, `margin` inside.
pub fn inset<R>(ui: &mut Ui, _t: &Theme, fill: Color32, margin: i8, f: impl FnOnce(&mut Ui) -> R) -> R {
    let raw = ui.as_raw();
    let (inner, ()) = with_body(f, |body, user| unsafe { (api().oak_inset)(raw, color_in(fill), margin as f32, body, user, std::ptr::null_mut()) });
    inner.expect("Oak didn't draw the box")
}

/// A drop-down's header row, `h` tall: a chevron, its title and `count`,
/// `right` at the right. A click opens or closes it (unless not `enabled`:
/// then it stays closed). Returns whether it is open, and the row's
/// response.
#[allow(clippy::too_many_arguments)]
pub fn drop_head(ui: &mut Ui, _t: &Theme, id: Id, h: f32, title: &str, count: &str, right: (&str, Color32), enabled: bool) -> (bool, Response) {
    let mut open = is_open(ui, id, false);
    let mut out = OakResponse::default();
    let flags = unsafe { (api().oak_drop_head)(ui.as_raw(), id.value(), h, s(title), s(count), s(right.0), color_in(right.1), enabled, open && enabled, &mut out) };
    if enabled && flags & OAK_CLICKED != 0 {
        open = !open;
        set_open(ui, id, open);
    }
    (open && enabled, Response::of(out))
}

/// Whether the thing `id` is open (`default` until it is toggled).
pub fn is_open(ui: &Ui, id: Id, default: bool) -> bool {
    ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(default)
}

pub fn set_open(ui: &Ui, id: Id, open: bool) {
    ui.data_mut(|d| d.insert_temp(id, open));
}
