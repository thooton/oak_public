//! Oak's look beyond single widgets (`widgets`): the sizes its pages are
//! laid out with, the boxes in a chat's corner, a list's rows, and the
//! parts of Oak's own pages a plugin draws with (markdown, pictures, a
//! chat's scrolling column). Oak draws them (`abi`'s `oak_` functions);
//! the theme arguments are kept for the code that passes them, and Oak
//! uses the theme in use.

use crate::abi::*;
use crate::egui::{self, Color32, FontFamily, FontId, Id, Rangef, Rect, Response, Ui, color_in, rect_in, rect_out, with_body, with_text_fn};
use crate::rt::{api, carry, rethrow, s};
use crate::theme::Theme;
use serde_json::Value;
use std::ffi::c_void;

/// The font family of Nerd Font icons.
pub const ICONS: &str = "icons";
/// The font family of code: code blocks, `code` and tool input and output.
pub const CODE: &str = "code";

pub fn code_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(CODE.into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

/// The heading row of Home's cells and of a page's card.
pub const HOME_HEAD: f32 = 34.0;
/// Room for the scroll bar right of a list's rows.
pub const BAR_ROOM: f32 = 12.0;
/// Between the composer row and the line above it, and between a corner
/// box and the window's edge.
pub const GAP: f32 = 12.0;
/// The session box's width, and the narrowest it gets.
pub const INFO_W: f32 = 300.0;
pub const INFO_MIN_W: f32 = 240.0;
/// The transcript's column, at most.
pub const TRANSCRIPT_W: f32 = 820.0;
/// The narrowest a conversation's column gets to make room for the session box.
pub const MIN_CHAT_W: f32 = 460.0;
/// A transcript row's height (a tool call's, a file's).
pub const ROW_H: f32 = 24.0;

/// The transcript's column in a panel spanning `area`: centred, at most
/// `TRANSCRIPT_W` wide, 16 points clear of the sides.
pub fn transcript_x(area: Rangef) -> Rangef {
    let w = TRANSCRIPT_W.min(area.span() - 32.0).max(100.0);
    let x = area.center() - w / 2.0;
    Rangef::new(x, x + w)
}

/// A conversation's column: `transcript_x`, moved right (and narrowed, down
/// to `MIN_CHAT_W`) when the room left of it is too small for the session
/// box, so the box shows at Oak's default window size too.
pub fn chat_column(area: Rangef) -> Rangef {
    let col = transcript_x(area);
    let x = area.min + GAP + INFO_MIN_W + 8.0;
    if col.min >= x {
        return col;
    }
    let w = (col.max - col.min).min(area.max - 16.0 - x);
    if w >= MIN_CHAT_W { Rangef::new(x, x + w) } else { col }
}

/// How wide a box in a chat's top left corner is, in `area` (the chat's):
/// None when the window is too narrow for one beside the transcript.
pub fn corner_width(area: Rect) -> Option<f32> {
    let w = unsafe { (api().oak_corner_width)(&rect_in(area)) };
    (w > 0.0).then_some(w)
}

/// The chat's top left corner, over the transcript: what `f` adds there,
/// 8 points apart.
pub fn corner_area(_ctx: &egui::Context, name: &str, area: Rect, f: impl FnOnce(&mut Ui)) {
    with_body(f, |body, user| unsafe { (api().oak_corner_area)(s(name), &rect_in(area), body, user) });
}

/// A box in the session column, `w` wide: `pad_y` above and below what
/// `f` adds, `gap` between its rows.
pub fn panel_box(ui: &mut Ui, _t: &Theme, w: f32, pad_y: i8, gap: f32, f: impl FnOnce(&mut Ui)) {
    let raw = ui.as_raw();
    with_body(f, |body, user| unsafe { (api().oak_panel_box)(raw, w, pad_y as f32, gap, body, user) });
}

/// A caption and its value, at the row's two ends.
pub fn kv(ui: &mut Ui, _t: &Theme, key: &str, value: &str, color: Color32) -> Response {
    let mut out = OakResponse::default();
    unsafe { (api().oak_kv)(ui.as_raw(), s(key), s(value), color_in(color), &mut out) };
    Response::of(out)
}

/// A thin bar `frac` full.
pub fn meter(ui: &mut Ui, _t: &Theme, w: f32, frac: f32, color: Color32) {
    unsafe { (api().oak_meter)(ui.as_raw(), w, frac, color_in(color)) };
}

/// A spinner in `rect`: an arc going round.
pub fn paint_spinner(p: &egui::Painter, rect: Rect, color: Color32) {
    unsafe { (api().oak_spinner)(p.raw(), &rect_in(rect), color_in(color)) };
}

/// One of Home's lists: `n` rows 58 points tall in a scroll area, a line
/// between them, the one under the pointer lit. `row` draws row `i` in
/// its rectangle, with its click.
pub fn list_rows(ui: &mut Ui, _t: &Theme, salt: &str, n: usize, empty: &str, mut row: impl FnMut(&mut Ui, usize, Rect, Response)) {
    unsafe extern "C" fn draw<F: FnMut(&mut Ui, usize, Rect, Response)>(user: *mut c_void, ui: *mut OakUi, index: usize, rect: *const OakRect, row: *const OakResponse) {
        let f = unsafe { &mut *(user as *mut F) };
        let (rect, resp) = unsafe { (rect_out(*rect), Response::of(*row)) };
        carry(|| f(&mut Ui::raw(ui), index, rect, resp));
    }
    fn tramp<F: FnMut(&mut Ui, usize, Rect, Response)>(_: &F) -> OakRow {
        draw::<F>
    }
    let call = tramp(&row);
    unsafe { (api().oak_list_rows)(ui.as_raw(), s(salt), n, s(empty), call, &mut row as *mut _ as *mut c_void) };
    rethrow();
}

/// A `list_rows` row's title: brighter when hovered.
pub fn row_title_color(t: &Theme, hovered: bool) -> Color32 {
    if hovered { t.ink } else { t.ink2.lerp_to_gamma(t.ink, 0.5) }
}

/// "5 KB", "2.4 MB": a file's size.
pub fn size_text(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n => format!("{} KB", n.div_ceil(1024).max(1)),
    }
}

/// The start of a string.
pub trait Head {
    /// Its first `n` characters.
    fn head(&self, n: usize) -> String;
}

impl Head for str {
    fn head(&self, n: usize) -> String {
        self.chars().take(n).collect()
    }
}

/// `s` on one line: every run of white space a single space.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Markdown, as a transcript shows it, selectable; `key` tells this text
/// from the others on the page.
pub fn markdown(ui: &mut Ui, _t: &Theme, text: &str, key: (usize, usize)) {
    unsafe { (api().oak_markdown)(ui.as_raw(), s(text), key.0 as u64, key.1 as u64, 0) };
}

/// `markdown`, its code blocks colored for their language.
pub fn markdown_highlighted(ui: &mut Ui, _t: &Theme, text: &str, key: (usize, usize)) {
    unsafe { (api().oak_markdown)(ui.as_raw(), s(text), key.0 as u64, key.1 as u64, OAK_MARKDOWN_HIGHLIGHT) };
}

/// The bar under a message (`msg`, its rectangle): Copy, and when it was sent.
pub fn message_bar(ui: &mut Ui, _t: &Theme, msg: Rect, id: Id, copy_text: impl Fn() -> String, at: Option<i64>) {
    let raw = ui.as_raw();
    with_text_fn(&copy_text, |copy, user| unsafe { (api().oak_message_bar)(raw, &rect_in(msg), id.value(), copy, user, at.unwrap_or(0)) });
}

/// An image block, at most `max_h` tall; a click opens it large. None:
/// the block isn't a picture Oak can show.
pub fn image(ui: &mut Ui, block: &Value, max_h: f32) -> Option<Response> {
    let mut out = OakResponse::default();
    let shown = unsafe { (api().oak_image)(ui.as_raw(), s(&block.to_string()), max_h, &mut out) };
    shown.then(|| Response::of(out))
}

/// The right-click menu of a message in `rect`: Copy (the selection, or
/// `message`), and what `extra` adds.
pub fn copy_menu(ui: &mut Ui, id: Id, rect: Rect, message: Option<&dyn Fn() -> String>, extra: Option<&dyn Fn(&mut Ui)>) {
    let raw = ui.as_raw();
    let rect = rect_in(rect);
    let with_extra = |text: Option<OakTextFn>, text_user: *mut c_void| match extra {
        Some(extra) => {
            with_body(|ui| extra(ui), |body, user| unsafe { (api().oak_copy_menu)(raw, id.value(), &rect, text, text_user, Some(body), user) });
        }
        None => unsafe { (api().oak_copy_menu)(raw, id.value(), &rect, text, text_user, None, std::ptr::null_mut()) },
    };
    match message {
        Some(message) => with_text_fn(message, |text, user| with_extra(Some(text), user)),
        None => with_extra(None, std::ptr::null_mut()),
    }
}

/// A chat drawn as widgets in the conversation's column of `ui`'s page,
/// kept scrolled to the newest: `f` lays it out top down, its rows `gap`
/// apart, under a 16-point margin.
pub fn chat_scroll(ui: &mut Ui, _t: &Theme, salt: impl std::hash::Hash + std::fmt::Debug, gap: f32, f: impl FnOnce(&mut Ui)) {
    let raw = ui.as_raw();
    with_body(f, |body, user| unsafe { (api().oak_chat_scroll)(raw, Id::unique(salt).value(), gap, body, user) });
}
