//! Oak's look beyond single widgets (`widgets`): the sizes its pages are
//! laid out with, the boxes in a chat's corner, a list's rows, and the
//! parts of Oak's own pages a plugin draws with (markdown, pictures, a
//! chat's scrolling column), which run in Oak (`Host`).

use crate::egui::{self, Align, Color32, FontFamily, FontId, Id, Layout, Margin, Rangef, Rect, RichText, ScrollArea, Sense, Stroke, Ui, vec2};
use crate::theme::Theme;
use crate::widgets::caption;
use serde_json::Value;

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
    let room = chat_column(area.x_range()).min - 8.0 - (area.left() + GAP);
    (room >= INFO_MIN_W).then(|| room.min(INFO_W))
}

/// The chat's top left corner, over the transcript: what `f` adds there,
/// 8 points apart.
pub fn corner_area(ctx: &egui::Context, name: &str, area: Rect, f: impl FnOnce(&mut Ui)) {
    egui::Area::new(Id::unique(name)).order(egui::Order::Middle).fixed_pos(area.min + vec2(GAP, GAP)).show(ctx, |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        f(ui);
    });
}

/// A box in the session column, `w` wide: `pad_y` above and below what
/// `f` adds, `gap` between its rows.
pub fn panel_box(ui: &mut Ui, t: &Theme, w: f32, pad_y: i8, gap: f32, f: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).inner_margin(Margin::symmetric(12, pad_y)).show(ui, |ui| {
        ui.set_width(w - 26.0);
        ui.spacing_mut().item_spacing.y = gap;
        f(ui);
    });
}

/// A caption and its value, at the row's two ends.
pub fn kv(ui: &mut Ui, t: &Theme, key: &str, value: &str, color: Color32) -> egui::Response {
    ui.horizontal(|ui| {
        caption(ui, key, t.ink3);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new(value).small().color(color)));
    })
    .response
}

/// A thin bar `frac` full.
pub fn meter(ui: &mut Ui, t: &Theme, w: f32, frac: f32, color: Color32) {
    let (bar, _) = ui.allocate_exact_size(vec2(w, 3.0), Sense::hover());
    ui.painter().rect_filled(bar, 0.0, t.panel2);
    let fill = Rect::from_min_size(bar.min, vec2(bar.width() * frac.clamp(0.0, 1.0), bar.height()));
    ui.painter().rect_filled(fill, 0.0, color);
}

/// egui's spinner, with a painter: an arc going round, its length waxing
/// and waning.
pub fn paint_spinner(p: &egui::Painter, rect: Rect, color: Color32) {
    p.ctx().request_repaint();
    let radius = rect.width().min(rect.height()) * 0.5 - 1.5;
    let time = p.ctx().input(|i| i.time);
    let start = time * std::f64::consts::TAU;
    let end = start + 240f64.to_radians() * time.sin();
    let n = 24;
    let points: Vec<egui::Pos2> = (0..n)
        .map(|i| {
            let a = start + (end - start) * i as f64 / n as f64;
            rect.center() + radius * vec2(a.cos() as f32, a.sin() as f32)
        })
        .collect();
    p.add(egui::Shape::line(points, Stroke::new(2.0, color)));
}

/// One of Home's lists (the conversations, a plugin's): `n` rows 58 points tall in a
/// scroll area, a line between them, the one under the pointer lit. `row`
/// draws row `i` in its rectangle, with its click.
pub fn list_rows(ui: &mut Ui, t: &Theme, salt: &str, n: usize, empty: &str, mut row: impl FnMut(&mut Ui, usize, Rect, egui::Response)) {
    if n == 0 {
        crate::widgets::center_note(ui, t, ui.max_rect(), empty);
        return;
    }
    let soft = Color32::from_rgba_unmultiplied(t.ink.r(), t.ink.g(), t.ink.b(), 0x14);
    // Only the rows in view are laid out (a long list cost a frame
    // milliseconds); the rows touch, so the spacing `show_rows` steps by is
    // none.
    let spacing = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show_rows(ui, 58.0, n, |ui, shown| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width() - BAR_ROOM;
        for i in shown {
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 0.0, t.tool_hover);
            }
            if i + 1 < n {
                ui.painter().hline(rect.left() + 12.0..=rect.right() - 12.0, rect.bottom() - 0.5, Stroke::new(1.0, soft));
            }
            row(ui, i, rect, resp.on_hover_cursor(egui::CursorIcon::PointingHand));
        }
    });
    ui.spacing_mut().item_spacing.y = spacing;
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
pub fn markdown(ui: &mut Ui, t: &Theme, text: &str, key: (usize, usize)) {
    crate::host().markdown(ui, t, text, key, false);
}

/// `markdown`, its code blocks colored for their language.
pub fn markdown_highlighted(ui: &mut Ui, t: &Theme, text: &str, key: (usize, usize)) {
    crate::host().markdown(ui, t, text, key, true);
}

/// The bar under a message (`msg`, its rectangle): Copy, and when it was sent.
pub fn message_bar(ui: &mut Ui, t: &Theme, msg: Rect, id: Id, copy_text: impl Fn() -> String, at: Option<i64>) {
    crate::host().message_bar(ui, t, msg, id, &copy_text, at);
}

/// An image block, at most `max_h` tall; a click opens it large.
pub fn image(ui: &mut Ui, block: &Value, max_h: f32) -> Option<egui::Response> {
    crate::host().image(ui, block, max_h)
}

/// The right-click menu of a message in `rect`: Copy (the selection, or
/// `message`), and what `extra` adds.
pub fn copy_menu(ui: &mut Ui, id: Id, rect: Rect, message: Option<&dyn Fn() -> String>, extra: Option<&dyn Fn(&mut Ui)>) {
    crate::host().copy_menu(ui, id, rect, message, extra);
}

/// A chat drawn as widgets in the conversation's column of `ui`'s page,
/// kept scrolled to the newest: `f` lays it out top down, its rows `gap`
/// apart, under a 16-point margin.
pub fn chat_scroll(ui: &mut Ui, t: &Theme, salt: impl std::hash::Hash + std::fmt::Debug, gap: f32, f: impl FnOnce(&mut Ui)) {
    let mut f = Some(f);
    crate::host().chat_scroll(ui, t, Id::unique(salt), gap, &mut |ui| {
        if let Some(f) = f.take() {
            f(ui);
        }
    });
}
