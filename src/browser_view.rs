//! The browser's view (`browser.rs` is the browser itself). Only the model
//! opens and closes a conversation's browser (BROWSER action=open and
//! close), and it fades in and out as it does. While it is open, a preview
//! of the page sits beside the transcript, between its column and the
//! minimap (over the transcript's corner when there is no room there),
//! see-through until the pointer is on it. A click on it (or Ctrl+Shift+B)
//! expands it into a lightbox over the window, as an image opens, where the
//! user uses the page: a toolbar (back, forward, reload, the address, back
//! to the preview) over the page, which takes the pointer, wheel and keys.
//! The X, a click beside it, Ctrl+Shift+B, or Esc while the page doesn't
//! have the focus go back to the preview. The page keeps its own viewport
//! (`browser.rs`) and is scaled to fit either way. The model's pointer
//! shows on the page as Oak's green cursor, gliding to where it goes.

use super::*;
use crate::browser::{self, Instance, Status};
use egui::TextureHandle;
use std::sync::Arc;

const BAR_H: f32 = 34.0;
/// Seconds the browser takes to fade in as it opens, and out as it closes.
const FADE: f32 = 0.2;
/// Seconds the preview takes to grow into the lightbox, and back.
const EXPAND: f32 = 0.16;
/// The preview's widest, and the narrowest that fits beside the transcript.
const PREVIEW_MAX_W: f32 = 360.0;
const PREVIEW_MIN_W: f32 = 170.0;
/// How opaque the preview is at rest beside the transcript, and over it.
pub(crate) const REST_ALPHA: f32 = 0.6;
pub(crate) const REST_ALPHA_OVER: f32 = 0.4;
/// Seconds the model's cursor stays after its last move (while no call of
/// its runs), then fades.
const CURSOR_STAYS: f32 = 20.0;
/// The caption line under the lightbox's page.
const CAPTION_H: f32 = 28.0;

/// The toolbar's icons, drawn as strokes of one weight in boxes of one
/// size (the icon font's glyphs come in different sizes and weights).
#[derive(Clone, Copy)]
enum Icon {
    Back,
    Forward,
    Reload,
    /// An x: Stop while a page loads.
    Cross,
    /// Back to the preview: a bar, as a window's minimize.
    Shrink,
}

/// The pointer's state over the page, between frames.
#[derive(Clone, Default)]
struct Pointer {
    /// A button went down on the page and hasn't come up: moves outside it
    /// still go to the page (a drag), and so does the release.
    held: bool,
    /// For double and triple clicks: the last press, where, which button,
    /// and how many in a row.
    last: Option<(f64, egui::Pos2, egui::PointerButton, i64)>,
}

/// A `<select>`'s options, open in Oak (CSS point to pick at, its options).
#[derive(Clone)]
struct OpenSelect {
    at: egui::Pos2,
    select: browser::Select,
}

/// A conversation's view between frames.
#[derive(Clone, Default)]
struct ViewState {
    expanded: bool,
    /// The preview where it was last drawn (the lightbox grows from it),
    /// and the transcript it was placed beside.
    preview: Option<Rect>,
    beside: Option<Rect>,
    /// The frame this was worked out in.
    pass: u64,
    /// How present the browser is, fading in or out (0 to 1).
    alpha: f32,
    /// What was shown last, kept to fade out after the browser closes:
    /// the page's picture and the viewport's size (CSS pixels).
    tex: Option<TextureHandle>,
    size: (u32, u32),
    /// When the page's first picture came (`ctx` time): it fades in over
    /// "Starting the browser".
    page_at: Option<f64>,
    /// When the browser was found gone (`ctx` time).
    gone_at: Option<f64>,
}

fn state_id(conv: &str) -> Id {
    Id::unique(("browser-view", conv))
}

fn page_id(conv: &str) -> Id {
    Id::unique(("browser-page", conv))
}

fn expand_id(conv: &str) -> Id {
    state_id(conv).with("expand")
}

/// Oak's green, whatever the theme: the model's cursor.
fn oak_green() -> Color32 {
    Theme::oak().accent
}

/// The view's state for `conv` this frame, worked out once a frame: how
/// present the browser is (fading in after it opened, out after it closed;
/// a browser that closed while its conversation wasn't shown just goes),
/// its newest frame uploaded, and what it should render (`Instance::shown`).
fn tick(ctx: &egui::Context, conv: &str) -> (Option<Arc<Instance>>, ViewState) {
    let inst = browser::get(conv);
    let mut s = ctx.data(|d| d.get_temp::<ViewState>(state_id(conv))).unwrap_or_default();
    let pass = ctx.cumulative_pass_nr();
    if s.pass == pass && s.pass != 0 {
        return (inst, s);
    }
    let seen_last = s.pass + 1 >= pass;
    s.pass = pass;
    match &inst {
        Some(i) => {
            s.gone_at = None;
            s.alpha = (i.opened.elapsed().as_secs_f32() / FADE).min(1.0);
            upload(ctx, i);
            if let Some(t) = i.texture.lock().unwrap().clone() {
                s.tex = Some(t);
                s.page_at.get_or_insert(ctx.input(|i| i.time));
            }
            s.size = i.viewport_css();
            let (_, page, k) = lightbox_layout(lightbox_area(ctx), s.size);
            let ppp = ctx.pixels_per_point();
            let cast = if s.expanded { page.size() } else { s.preview.map_or(vec2(480.0, 270.0), |r| r.size()) } * ppp;
            i.shown(k * ppp, [cast.x.ceil() as u32, cast.y.ceil() as u32]);
        }
        None if s.alpha > 0.0 => {
            if s.gone_at.is_none() && !seen_last {
                s = ViewState { pass, ..Default::default() };
            } else {
                let now = ctx.input(|i| i.time);
                let gone = *s.gone_at.get_or_insert(now);
                s.alpha = 1.0 - (now - gone) as f32 / FADE;
                if s.alpha <= 0.0 {
                    s = ViewState { pass, ..Default::default() };
                    ctx.memory_mut(|m| m.surrender_focus(page_id(conv)));
                }
            }
        }
        None => s.expanded = false,
    }
    // (Without a browser there is nothing to fade in: no frames then.)
    if (s.alpha > 0.0 && s.alpha < 1.0) || (inst.is_some() && page_in(ctx, &s) < 1.0) {
        ctx.request_repaint();
    }
    store(ctx, conv, &s);
    (inst, s)
}

/// How far the page's first picture has faded in (0 to 1).
fn page_in(ctx: &egui::Context, s: &ViewState) -> f32 {
    s.page_at.map_or(0.0, |at| ((ctx.input(|i| i.time) - at) as f32 / FADE).clamp(0.0, 1.0))
}

fn store(ctx: &egui::Context, conv: &str, s: &ViewState) {
    ctx.data_mut(|d| d.insert_temp(state_id(conv), s.clone()));
}

/// The browser's newest frame, into its texture.
fn upload(ctx: &egui::Context, inst: &Instance) {
    let Some(img) = inst.view.lock().unwrap().frame.take() else { return };
    let mut tex = inst.texture.lock().unwrap();
    match tex.as_mut() {
        Some(h) => h.set(img, egui::TextureOptions::LINEAR),
        None => *tex = Some(ctx.load_texture(format!("browser-{}", inst.conv), img, egui::TextureOptions::LINEAR)),
    }
}

fn set_expanded(ctx: &egui::Context, conv: &str, s: &mut ViewState, on: bool) {
    s.expanded = on;
    // Expanded, the keys go to the page (not the message box); back as a
    // preview, nothing of it keeps them.
    ctx.memory_mut(|m| if on { m.request_focus(page_id(conv)) } else { m.surrender_focus(page_id(conv)) });
}

/// The window below the title bar: what the lightbox covers.
fn lightbox_area(ctx: &egui::Context) -> Rect {
    let screen = ctx.content_rect();
    Rect::from_min_max(egui::pos2(screen.left(), screen.top() + TITLE_H), screen.max)
}

/// Where the lightbox puts a `size` viewport (CSS pixels) in `area`: the
/// frame (the toolbar and the page), the page, and its scale (points per
/// CSS pixel). Scaled to fit, never above 1:1, with the caption under it.
fn lightbox_layout(area: Rect, size: (u32, u32)) -> (Rect, Rect, f32) {
    let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let margin = vec2((area.width() * 0.03).max(24.0), (area.height() * 0.03).max(20.0));
    let room = area.shrink2(margin);
    let room_h = (room.height() - BAR_H - CAPTION_H).max(40.0);
    let k = (room.width() / w).min(room_h / h).clamp(0.02, 1.0);
    let frame_size = vec2(w * k, h * k + BAR_H);
    let center = egui::pos2(room.center().x, room.top() + (room.height() - CAPTION_H) / 2.0);
    let frame = Rect::from_center_size(center, frame_size);
    let page = Rect::from_min_max(frame.min + vec2(0.0, BAR_H), frame.max);
    (frame, page, k)
}

/// Where the preview of a `size` viewport goes in a transcript `view`
/// whose column is `col` and whose minimap (or scroll bar) starts at
/// `right`, from `top`: in the room between the column and the minimap,
/// else over the transcript's corner (true). Its shape is the viewport's.
pub(crate) fn preview_layout(view: Rect, col: Rangef, right: f32, top: f32, size: (u32, u32)) -> (Rect, bool) {
    let aspect = size.0.max(1) as f32 / size.1.max(1) as f32;
    let max_h = (view.height() * 0.4).clamp(80.0, 300.0);
    let fit = |w: f32| {
        let h = (w / aspect).min(max_h);
        vec2(h * aspect, h)
    };
    let right = right - 14.0;
    let room = right - (col.max + 20.0);
    let (size, over) = if room >= PREVIEW_MIN_W { (fit(room.min(PREVIEW_MAX_W)), false) } else { (fit((view.width() * 0.3).clamp(140.0, 260.0)), true) };
    (Rect::from_min_size(egui::pos2(right - size.x, top), size), over)
}

impl App {
    /// Windowed protocols also belong to subagents: select the actual
    /// shown conversation, not the parent/browser-only conversation.
    pub(crate) fn program_conv(&self) -> Option<String> {
        if self.page != Page::Chats { return None }
        let Screen::Chat(id) = &self.screen else { return None };
        let c = self.sessions.get(id)?.conv.lock().unwrap();
        (!c.chat && c.side_chat_of.is_none()).then(|| id.clone())
    }

    /// The conversation whose browser the shown screen can have: a
    /// conversation the user started (not a subagent, a side chat, a
    /// plugin's tab or a cloud session).
    pub(crate) fn browser_conv(&self) -> Option<String> {
        if self.page != Page::Chats {
            return None;
        }
        let Screen::Chat(id) = &self.screen else { return None };
        let s = self.sessions.get(id)?;
        {
            let c = s.conv.lock().unwrap();
            (c.parent.is_none() && c.side_chat_of.is_none()).then(|| id.clone())
        }
    }

    /// Before the chats page's panels: Ctrl+Shift+B, and the lightbox
    /// while it is expanded (first, so it gets Esc, the wheel and the keys
    /// before the transcript and the message box).
    pub(crate) fn browser_lightbox(&mut self, ctx: &egui::Context, t: &Theme) {
        let Some(conv) = self.browser_conv() else { return };
        let (inst, mut s) = tick(ctx, &conv);
        if s.alpha <= 0.0 {
            return;
        }
        if inst.is_some() && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::B)) {
            let on = !s.expanded;
            set_expanded(ctx, &conv, &mut s, on);
        }
        let e = ctx.animate_bool_with_time(expand_id(&conv), s.expanded, EXPAND);
        if e > 0.0 {
            lightbox(ctx, t, &conv, inst.as_ref(), &mut s, e);
        }
        store(ctx, &conv, &s);
    }

    /// After the transcript (drawn in `view`): the preview beside it.
    /// Returns where it is while shown (Blender's live view goes under it).
    pub(crate) fn browser_preview(&mut self, ctx: &egui::Context, t: &Theme, view: Rect) -> Option<Rect> {
        let conv = self.browser_conv()?;
        let (inst, mut s) = tick(ctx, &conv);
        if s.alpha <= 0.0 {
            return None;
        }
        let (col, right, top) = preview_slot(ctx, &conv, view);
        let (rect, over) = preview_layout(view, col, right, top, s.size);
        (s.preview, s.beside) = (Some(rect), Some(view));
        let e = ctx.animate_bool_with_time(expand_id(&conv), s.expanded, EXPAND);
        let shown = s.alpha * (1.0 - e);
        if shown > 0.0 {
            let id = state_id(&conv).with("preview");
            egui::Area::new(id).order(egui::Order::Middle).fixed_pos(rect.min).show(ctx, |ui| {
                let (r, resp) = ui.allocate_exact_size(rect.size(), Sense::click());
                let hover = ui.ctx().animate_bool_with_time(id.with("hover"), resp.hovered(), 0.12);
                let rest = if over { REST_ALPHA_OVER } else { REST_ALPHA };
                let mut p = ui.painter().clone();
                p.set_opacity(shown * (rest + (1.0 - rest) * hover));
                p.rect_filled(r.translate(vec2(0.0, 3.0)).expand(1.0), 6.0, Color32::from_black_alpha(70));
                page_picture(&p, t, None, r, 5.0);
                let mut pp = p.clone();
                pp.multiply_opacity(page_in(ui.ctx(), &s));
                page_picture(&pp, t, s.tex.as_ref(), r, 5.0);
                if let Some(inst) = &inst {
                    preview_overlays(ui, &p, t, inst, r, r.width() / s.size.0.max(1) as f32);
                }
                p.rect_stroke(r, 5.0, Stroke::new(1.0, if hover > 0.5 { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
                if let Some(inst) = &inst {
                    let (title, url) = {
                        let v = inst.view.lock().unwrap();
                        (v.title.clone(), shown_url(&v.url))
                    };
                    let what = [title, url].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                    let tip = if what.is_empty() { "Open the browser (Ctrl+Shift+B)".to_string() } else { format!("{what}\nClick to open (Ctrl+Shift+B)") };
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                        set_expanded(ui.ctx(), &conv, &mut s, true);
                    }
                }
            });
        }
        store(ctx, &conv, &s);
        (shown > 0.0).then_some(rect)
    }
}

/// Where previews beside a transcript in `view` go: its column, where the
/// minimap (or scroll bar) starts, and the top (under the find bar, when it
/// is open, as it is in the same corner).
pub(crate) fn preview_slot(ctx: &egui::Context, conv: &str, view: Rect) -> (Rangef, f32, f32) {
    let frame = chat_view::Frame::new(view);
    let right = frame.map.map_or(frame.track.left(), |m| m.left());
    let finding = ctx.data(|d| d.get_temp::<crate::find::Find>(Id::unique(("find", conv)))).is_some_and(|f| f.open);
    (frame.col, right, view.top() + if finding { 52.0 } else { 12.0 })
}

/// The page's picture filling `rect` (white until there is one).
fn page_picture(p: &egui::Painter, t: &Theme, tex: Option<&TextureHandle>, rect: Rect, round: f32) {
    let uv = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    match tex {
        Some(h) => {
            p.add(egui::epaint::RectShape::filled(rect, round, Color32::WHITE).with_texture(h.id(), uv));
        }
        None => {
            p.rect_filled(rect, round, t.panel);
        }
    }
}

/// Over the preview: how the browser is doing when it isn't showing a
/// page, a dialog waiting, and the model's cursor (`k`: points per CSS px).
fn preview_overlays(ui: &Ui, p: &egui::Painter, t: &Theme, inst: &Arc<Instance>, r: Rect, k: f32) {
    let (status, dialog) = {
        let v = inst.view.lock().unwrap();
        (v.status.clone(), v.dialog.is_some())
    };
    let line = |text: &str, color: Color32| {
        let g = p.layout(text.to_string(), FontId::proportional(11.5), color, r.width() - 20.0);
        p.galley(r.center() - g.size() / 2.0, g, color);
    };
    match status {
        Status::Starting => {
            p.rect_filled(r, 5.0, t.panel);
            line("Starting the browser…", t.ink3);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        Status::Failed(_) | Status::Quit(_) => {
            p.rect_filled(r, 5.0, t.panel);
            line("The browser stopped. Click to see why.", t.error);
        }
        Status::Ready => {
            if dialog {
                let g = p.layout_no_wrap("The page is waiting on a dialog".into(), FontId::proportional(11.0), Color32::WHITE);
                let tag = Rect::from_min_size(egui::pos2(r.left() + 6.0, r.bottom() - g.size().y - 12.0), g.size() + vec2(10.0, 6.0));
                p.rect_filled(tag, 4.0, Color32::from_black_alpha(200));
                p.galley(tag.min + vec2(5.0, 3.0), g, Color32::WHITE);
            }
            agent_overlays(ui.ctx(), p, inst, r, k, 0.75, false);
        }
    }
}

/// The model's doing over the page in `rect` (`k` points per CSS px): a
/// ring where it just clicked, and its cursor (`size` times the usual,
/// with a name tag when `tag`).
fn agent_overlays(ctx: &egui::Context, p: &egui::Painter, inst: &Arc<Instance>, rect: Rect, k: f32, size: f32, tag: bool) {
    let (ptr, at) = {
        let v = inst.view.lock().unwrap();
        (v.agent_ptr, v.agent_at)
    };
    let green = oak_green();
    let p = p.with_clip_rect(rect.intersect(p.clip_rect()));
    if let Some((at, when)) = at {
        match std::time::Instant::now().checked_duration_since(when).map(|d| d.as_secs_f32()) {
            Some(age) if age < 0.8 => {
                let a = 1.0 - age / 0.8;
                p.circle_stroke(rect.min + at.to_vec2() * k, (5.0 + 16.0 * age) * size, Stroke::new(2.0, green.gamma_multiply(a)));
                ctx.request_repaint();
            }
            Some(_) => {}
            None => ctx.request_repaint(),
        }
    }
    let Some(ptr) = ptr else { return };
    let (at, moving) = ptr.now();
    let idle = ptr.at.elapsed().saturating_sub(ptr.glide).as_secs_f32();
    let alpha = if inst.agent_busy() { 1.0 } else { 1.0 - ((idle - CURSOR_STAYS) / 0.5).clamp(0.0, 1.0) };
    if moving || (alpha > 0.0 && alpha < 1.0) {
        ctx.request_repaint();
    } else if alpha > 0.0 {
        ctx.request_repaint_after(std::time::Duration::from_secs_f32((CURSOR_STAYS - idle).max(0.05)));
    }
    if alpha > 0.0 {
        paint_cursor(&p, rect.min + at.to_vec2() * k, size, tag, alpha);
    }
}

/// Oak's cursor: a green arrow with its tip at `tip` (`size` times 17
/// points tall), and when `tag`, a green "Oak" tag beside it.
fn paint_cursor(p: &egui::Painter, tip: egui::Pos2, size: f32, tag: bool, alpha: f32) {
    let green = oak_green().gamma_multiply(alpha);
    let edge = Color32::from_black_alpha((220.0 * alpha) as u8);
    // The arrow's outline; filled as triangles (it isn't convex).
    let pts: Vec<egui::Pos2> = [(0.0, 0.0), (0.0, 15.0), (3.6, 11.6), (6.1, 17.4), (8.4, 16.4), (5.9, 10.8), (10.8, 10.8)]
        .iter()
        .map(|&(x, y)| tip + vec2(x, y) * size * 1.1)
        .collect();
    let mut mesh = egui::Mesh::default();
    for q in &pts {
        mesh.colored_vertex(*q, green);
    }
    for [a, b, c] in [[0, 1, 2], [0, 2, 5], [0, 5, 6], [2, 3, 4], [2, 4, 5]] {
        mesh.add_triangle(a, b, c);
    }
    p.add(mesh);
    p.add(egui::Shape::closed_line(pts.clone(), Stroke::new(1.2, edge)));
    if tag {
        let g = p.layout_no_wrap("Oak".into(), FontId::proportional(11.0 * size.max(0.8)), Color32::BLACK);
        let r = Rect::from_min_size(tip + vec2(12.0, 16.0) * size, g.size() + vec2(10.0, 4.0));
        p.rect_filled(r, 4.0, green);
        p.galley(r.min + vec2(5.0, 2.0), g, Color32::from_black_alpha((235.0 * alpha) as u8));
    }
}

/// The lightbox, `e` of the way open: the page growing from the preview
/// over a dimming window, then, open, the toolbar, the page to use and its
/// caption.
fn lightbox(ctx: &egui::Context, t: &Theme, conv: &str, inst: Option<&Arc<Instance>>, s: &mut ViewState, e: f32) {
    let area = lightbox_area(ctx);
    let (frame, page, k) = lightbox_layout(area, s.size);
    let ease = 1.0 - (1.0 - e).powi(3);
    let a = s.alpha;
    let full = e >= 1.0 && a >= 1.0;
    let mut collapse = false;
    egui::Area::new(state_id(conv).with("lightbox")).order(egui::Order::Foreground).fixed_pos(area.min).show(ctx, |ui| {
        let (rect, backdrop) = ui.allocate_exact_size(area.size(), Sense::click());
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, Color32::from_black_alpha((200.0 * ease * a) as u8));
        let inst = match inst {
            Some(i) if full => i,
            _ => {
                // On its way in or out: the picture alone, from the preview to its place.
                let from = s.preview.unwrap_or(Rect::from_center_size(page.center(), page.size() * 0.9));
                let r = Rect::from_min_max(from.min.lerp(page.min, ease), from.max.lerp(page.max, ease));
                let mut fp = p.clone();
                fp.set_opacity(a);
                page_picture(&fp, t, s.tex.as_ref(), r, 0.0);
                return;
            }
        };
        // Clicks on the frame's bare parts aren't clicks beside it.
        ui.interact(frame, state_id(conv).with("frame"), Sense::click());
        if !ui.memory(|m| m.has_focus(page_id(conv))) && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            collapse = true;
        }
        p.rect_stroke(frame, 0.0, Stroke::new(1.0, t.line), egui::StrokeKind::Outside);
        let bar = Rect::from_min_max(frame.min, egui::pos2(frame.right(), frame.top() + BAR_H));
        collapse |= toolbar(ui, t, inst, bar);
        page_view(ui, t, inst, page, k);
        let pct = (k * 100.0).round();
        let caption = format!("{}×{} viewport{}", s.size.0, s.size.1, if pct < 100.0 { format!(", shown at {pct}%") } else { String::new() });
        let cy = page.bottom() + CAPTION_H / 2.0 + 2.0;
        p.text(egui::pos2(page.left(), cy), Align2::LEFT_CENTER, caption, FontId::proportional(12.0), t.ink3);
        p.text(egui::pos2(page.right(), cy), Align2::RIGHT_CENTER, "Click outside or Ctrl+Shift+B to go back to the preview", FontId::proportional(12.0), t.ink3);
        collapse |= close_button(ui, t, rect, conv);
        collapse |= backdrop.clicked();
    });
    if collapse {
        set_expanded(ctx, conv, s, false);
    }
}

/// The X in the lightbox's corner; true when clicked.
fn close_button(ui: &Ui, t: &Theme, rect: Rect, conv: &str) -> bool {
    let p = ui.painter();
    let x_rect = Rect::from_center_size(egui::pos2(rect.right() - 32.0, rect.top() + 32.0), vec2(36.0, 36.0));
    let resp = ui.interact(x_rect, state_id(conv).with("close"), Sense::click());
    let lit = resp.hovered();
    p.circle_filled(x_rect.center(), 18.0, Color32::from_white_alpha(if lit { 40 } else { 16 }));
    let (c, s) = (x_rect.center(), 6.5);
    let stroke = Stroke::new(1.8, if lit { t.ink } else { t.ink2 });
    p.line_segment([c + vec2(-s, -s), c + vec2(s, s)], stroke);
    p.line_segment([c + vec2(-s, s), c + vec2(s, -s)], stroke);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Back to the preview").clicked()
}

/// A toolbar button: an icon, dimmed when it can't be used.
fn bar_button(ui: &mut Ui, t: &Theme, rect: Rect, id: Id, icon: Icon, tip: &str, enabled: bool) -> bool {
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.painter().rect_filled(Rect::from_center_size(rect.center(), vec2(24.0, 24.0)), 4.0, t.panel2);
    }
    let color = if !enabled { t.ink4 } else if hovered { t.ink } else { t.ink2 };
    paint_icon(ui.painter(), rect.center(), icon, color);
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    resp.on_hover_text(tip).clicked() && enabled
}

/// `icon` centered on `c`, in a box about 12 points across.
fn paint_icon(p: &egui::Painter, c: egui::Pos2, icon: Icon, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    match icon {
        Icon::Back | Icon::Forward => {
            let d = if matches!(icon, Icon::Back) { -1.0 } else { 1.0 };
            let tip = c + vec2(6.0 * d, 0.0);
            p.line_segment([c - vec2(6.0 * d, 0.0), tip], stroke);
            p.add(egui::Shape::line(vec![tip + vec2(-4.5 * d, -4.5), tip, tip + vec2(-4.5 * d, 4.5)], stroke));
        }
        Icon::Reload => {
            // Clockwise from the right round to the upper right, into the
            // corner of an arrowhead there.
            let r = 5.5;
            let to = 300f32.to_radians();
            let n = 24;
            let corner = c + vec2(r, -2.4);
            let mut line: Vec<_> = (0..=n).map(|i| c + r * egui::Vec2::angled(to * i as f32 / n as f32)).collect();
            line.push(corner);
            p.add(egui::Shape::line(line, stroke));
            p.add(egui::Shape::line(vec![corner + vec2(0.0, -3.4), corner, corner + vec2(-3.4, 0.0)], stroke));
        }
        Icon::Cross => {
            let h = 4.5;
            p.line_segment([c + vec2(-h, -h), c + vec2(h, h)], stroke);
            p.line_segment([c + vec2(-h, h), c + vec2(h, -h)], stroke);
        }
        Icon::Shrink => {
            p.line_segment([c + vec2(-5.5, 4.0), c + vec2(5.5, 4.0)], stroke);
        }
    }
}

/// The toolbar over the page; true when its button back to the preview is clicked.
fn toolbar(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, bar: Rect) -> bool {
    let p = ui.painter().clone();
    p.rect_filled(bar, 0.0, t.panel);
    p.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, t.line));
    let (url, back, forward, loading, ready) = {
        let v = inst.view.lock().unwrap();
        (v.url.clone(), v.back, v.forward, v.loading, v.status == Status::Ready)
    };
    let conv = &inst.conv;
    let b = |i: f32| Rect::from_min_size(egui::pos2(bar.left() + 4.0 + i * 30.0, bar.top() + 2.0), vec2(30.0, BAR_H - 4.0));
    if bar_button(ui, t, b(0.0), Id::unique(("browser-back", conv)), Icon::Back, "Back", ready && back) {
        inst.go(-1);
    }
    if bar_button(ui, t, b(1.0), Id::unique(("browser-forward", conv)), Icon::Forward, "Forward", ready && forward) {
        inst.go(1);
    }
    let (icon, tip) = if loading { (Icon::Cross, "Stop") } else { (Icon::Reload, "Reload") };
    if bar_button(ui, t, b(2.0), Id::unique(("browser-reload", conv)), icon, tip, ready) {
        if loading { inst.stop_loading() } else { inst.reload() }
    }
    let shrink = Rect::from_min_size(egui::pos2(bar.right() - 34.0, bar.top() + 2.0), vec2(30.0, BAR_H - 4.0));
    let collapse = bar_button(ui, t, shrink, Id::unique(("browser-shrink", conv)), Icon::Shrink, "Back to the preview (Ctrl+Shift+B)", true);
    // The address: what the page is at, until the user edits it.
    let field = Rect::from_min_max(egui::pos2(b(3.0).left() + 4.0, bar.top() + 5.0), egui::pos2(shrink.left() - 6.0, bar.bottom() - 5.0));
    let id = Id::unique(("browser-address", conv));
    let text_id = id.with("text");
    let focused = ui.memory(|m| m.has_focus(id));
    let mut text = ui.data(|d| d.get_temp::<String>(text_id)).filter(|_| focused).unwrap_or_else(|| shown_url(&url));
    let resp = crate::text_menu::put(
        ui,
        field,
        TextEdit::singleline(&mut text).id(id).font(FontId::proportional(12.5)).hint_text("Search or enter address").margin(Margin::symmetric(8, 3)).desired_width(field.width()),
    );
    if resp.gained_focus() {
        // All of it selected, to type over.
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), id);
        }
    }
    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && ready {
        inst.navigate(&text);
        ui.data_mut(|d| d.remove::<String>(text_id));
    } else if ui.memory(|m| m.has_focus(id)) {
        ui.data_mut(|d| d.insert_temp(text_id, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(text_id));
    }
    collapse
}

/// The address as the bar shows it (about:blank as nothing).
fn shown_url(url: &str) -> String {
    if url == "about:blank" { String::new() } else { url.to_string() }
}

/// The page in the lightbox, at `rect` (`k` points per CSS pixel), taking
/// the user's input.
fn page_view(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, rect: Rect, k: f32) {
    let ctx = ui.ctx().clone();
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, Color32::WHITE);
    if let Some(h) = inst.texture.lock().unwrap().as_ref() {
        paint_page_texture(&p, h, rect);
    }
    let status = inst.view.lock().unwrap().status.clone();
    match status {
        Status::Ready => {}
        Status::Starting => {
            p.rect_filled(rect, 0.0, t.bg);
            let what = if inst.attached() { "Attaching to your tab…" } else { "Starting the browser…" };
            p.text(rect.center(), Align2::CENTER_CENTER, what, FontId::proportional(13.0), t.ink3);
            return;
        }
        Status::Failed(e) | Status::Quit(e) => {
            p.rect_filled(rect, 0.0, t.bg);
            let area = Rect::from_center_size(rect.center(), vec2((rect.width() - 40.0).min(420.0), 140.0));
            ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(e).color(t.error));
                    ui.add_space(10.0);
                    if ui.button(if inst.attached() { "Attach to the tab again" } else { "Restart the browser" }).clicked() {
                        browser::restart(&inst.conv, ui.ctx());
                    }
                });
            });
            return;
        }
    }
    let page_id = page_id(&inst.conv);
    let resp = ui.interact(rect, page_id, Sense::click_and_drag());
    let (dialog, cursor, select, notice, copied) = {
        let mut v = inst.view.lock().unwrap();
        (v.dialog.clone(), v.cursor, v.select.clone(), v.notice.clone(), v.copied.take())
    };
    if let Some(text) = copied {
        ctx.copy_text(text);
    }
    let sel_id = page_id.with("select");
    let open_select = ui.data(|d| d.get_temp::<OpenSelect>(sel_id));
    if dialog.is_none() && open_select.is_none() {
        forward_input(ui, inst, rect, k, page_id, resp.hovered(), select.as_ref());
        if resp.hovered() {
            ctx.set_cursor_icon(cursor);
        }
    }
    // The page's file chooser, opened by the user.
    if let Some(c) = inst.take_chooser() {
        let dialog = rfd::FileDialog::new();
        let files = if c.multiple { dialog.pick_files().unwrap_or_default() } else { dialog.pick_file().into_iter().collect() };
        inst.choose_files(&c, files);
    }
    agent_overlays(&ctx, &p, inst, rect, k, 1.0, true);
    if let Some((text, when)) = notice
        && when.elapsed().as_secs() < 6
    {
        let g = p.layout(text, FontId::proportional(12.0), t.ink, rect.width() - 40.0);
        let r = Rect::from_min_size(egui::pos2(rect.left() + 12.0, rect.bottom() - g.size().y - 22.0), g.size() + vec2(16.0, 10.0));
        p.rect_filled(r, 4.0, t.panel2);
        p.rect_stroke(r, 4.0, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
        p.galley(r.min + vec2(8.0, 5.0), g, t.ink);
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
    if let Some(d) = dialog {
        dialog_view(ui, t, inst, rect, &d);
    }
    if let Some(s) = open_select {
        select_view(ui, t, inst, rect, k, sel_id, &s);
    }
}

/// Screencast pixels are not necessarily device pixels: Chromium scales
/// them to the size asked for. Map the complete frame to the page's
/// rectangle, which is also where input is taken.
fn paint_page_texture(p: &egui::Painter, texture: &egui::TextureHandle, rect: Rect) {
    p.image(texture.id(), rect, Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
}

/// The user's pointer, wheel and keys, to the page at `rect` (`k` points
/// per CSS pixel). `hovered`: the page is what the pointer is on, so a
/// press there is the page's.
fn forward_input(ui: &mut Ui, inst: &Arc<Instance>, rect: Rect, k: f32, page_id: Id, hovered: bool, select: Option<&browser::Select>) {
    let events = ui.input(|i| i.events.clone());
    if forward_pointer(ui, inst, rect, k, page_id, hovered, select, &events) {
        forward_keys(ui, inst, page_id, &events);
    }
}

/// The pointer and the wheel, to the page; a press on its `<select>`
/// opens the options in Oak instead. True when the page has the focus.
#[allow(clippy::too_many_arguments)]
fn forward_pointer(ui: &mut Ui, inst: &Arc<Instance>, rect: Rect, k: f32, page_id: Id, hovered: bool, select: Option<&browser::Select>, events: &[egui::Event]) -> bool {
    let ptr_id = page_id.with("pointer");
    let mut ptr = ui.data(|d| d.get_temp::<Pointer>(ptr_id)).unwrap_or_default();
    let (time, down) = ui.input(|i| {
        let down = (i.pointer.button_down(egui::PointerButton::Primary) as i64)
            | (i.pointer.button_down(egui::PointerButton::Secondary) as i64) << 1
            | (i.pointer.button_down(egui::PointerButton::Middle) as i64) << 2;
        (i.time, down)
    });
    let css = |p: egui::Pos2| ((p - rect.min) / k).to_pos2();
    let mods_now = ui.input(|i| i.modifiers);
    let mut focused = ui.memory(|m| m.has_focus(page_id));
    for e in events {
        match e {
            egui::Event::PointerMoved(p) if rect.contains(*p) || ptr.held => {
                let held = if ptr.held { down } else { 0 };
                let button = if held & 1 != 0 { "left" } else if held & 2 != 0 { "right" } else if held & 4 != 0 { "middle" } else { "none" };
                inst.mouse("mouseMoved", css(*p), button, held, 0, browser::modifier_bits(mods_now));
            }
            egui::Event::PointerButton { pos, button, pressed: true, modifiers } if hovered && rect.contains(*pos) => {
                // Over a `<select>`: its options open in Oak instead.
                if *button == egui::PointerButton::Primary
                    && let Some(s) = select.filter(|s| !s.disabled)
                {
                    let at = css(*pos);
                    let [x, y, w, h] = s.rect;
                    if at.x >= x && at.x <= x + w && at.y >= y && at.y <= y + h {
                        ui.data_mut(|d| d.insert_temp(page_id.with("select"), OpenSelect { at: egui::pos2(x + w / 2.0, y + h / 2.0), select: s.clone() }));
                        continue;
                    }
                }
                ui.memory_mut(|m| m.request_focus(page_id));
                focused = true;
                let count = match ptr.last {
                    Some((t, p, b, n)) if b == *button && time - t < 0.5 && p.distance(*pos) < 5.0 => n % 3 + 1,
                    _ => 1,
                };
                ptr.last = Some((time, *pos, *button, count));
                ptr.held = true;
                inst.mouse("mousePressed", css(*pos), browser::button_name(*button), down, count, browser::modifier_bits(*modifiers));
            }
            egui::Event::PointerButton { pos, button, pressed: false, modifiers } if ptr.held || rect.contains(*pos) => {
                let count = ptr.last.map_or(1, |l| l.3);
                inst.mouse("mouseReleased", css(*pos), browser::button_name(*button), down, count, browser::modifier_bits(*modifiers));
                if down == 0 {
                    ptr.held = false;
                }
            }
            egui::Event::MouseWheel { unit, delta, modifiers, .. } => {
                let Some(at) = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p)) else { continue };
                let per = match unit {
                    egui::MouseWheelUnit::Point => 1.0,
                    egui::MouseWheelUnit::Line => 40.0,
                    egui::MouseWheelUnit::Page => rect.height(),
                };
                // egui's up is positive; the page's down is. The page
                // moves as far on screen as the wheel says.
                inst.wheel(css(at), -delta.x * per / k, -delta.y * per / k, browser::modifier_bits(*modifiers));
            }
            _ => {}
        }
    }
    // The wheel over the page is the page's: nothing else scrolls with it.
    if ui.rect_contains_pointer(rect) {
        ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
    }
    ui.data_mut(|d| d.insert_temp(ptr_id, ptr));
    focused
}

/// Keys go to the page while it has the focus: Tab and the arrows too,
/// and nothing else in Oak sees them (but Oak's own shortcuts).
fn forward_keys(ui: &mut Ui, inst: &Arc<Instance>, page_id: Id, events: &[egui::Event]) {
    ui.memory_mut(|m| m.set_focus_lock_filter(page_id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
    let sent: Vec<bool> = events.iter().map(|e| send_key(inst, e)).collect();
    if !sent.contains(&true) {
        return;
    }
    ui.input_mut(|inp| {
        let mut sent = sent.iter();
        inp.events.retain(|_| !sent.next().is_some_and(|s| *s));
        inp.keys_down.clear();
    });
}

/// Sends key, text or clipboard event `e` to the page; false when it
/// isn't one, or is one of Oak's own shortcuts.
fn send_key(inst: &Arc<Instance>, e: &egui::Event) -> bool {
    match e {
        egui::Event::Key { key, pressed, modifiers, .. } => {
            let oak = (modifiers.command && modifiers.shift && *key == Key::B) || (modifiers.alt && *key == Key::H) || (modifiers.command && !modifiers.shift && *key == Key::T) || crate::tabs::tab_key(*key, *modifiers).is_some();
            if oak {
                return false;
            }
            inst.key(key.name(), *pressed, browser::modifier_bits(*modifiers));
        }
        egui::Event::Text(text) => inst.text(text),
        egui::Event::Paste(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => inst.paste(text),
        egui::Event::Copy | egui::Event::Cut => inst.copy(matches!(e, egui::Event::Cut)),
        _ => return false,
    }
    true
}

/// The page's alert, confirm or prompt, over it.
fn dialog_view(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, rect: Rect, d: &browser::Dialog) {
    ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(110));
    let w = (rect.width() - 40.0).min(420.0);
    let text_id = Id::unique(("browser-prompt", &inst.conv));
    let mut text = ui.data(|m| m.get_temp::<String>(text_id)).unwrap_or_else(|| d.default.clone());
    let area = Rect::from_min_size(egui::pos2(rect.center().x - w / 2.0, rect.top() + 60.0), vec2(w, 200.0));
    let mut answer = None;
    ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
        egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).corner_radius(6.0).inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(w - 28.0);
            let host = inst.view.lock().unwrap().url.split('/').nth(2).unwrap_or("The page").to_string();
            ui.label(RichText::new(format!("{host} says")).color(t.ink3).size(11.5));
            ui.add_space(4.0);
            ui.label(RichText::new(if d.kind == "beforeunload" { "Leave the page? Changes may not be saved." } else { &d.message }).color(t.ink));
            ui.add_space(8.0);
            if d.kind == "prompt" {
                let r = crate::text_menu::add(ui, TextEdit::singleline(&mut text).desired_width(f32::INFINITY));
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    answer = Some(true);
                }
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                if ui.button(RichText::new(if d.kind == "beforeunload" { "Leave" } else { "OK" }).color(t.accent)).clicked() {
                    answer = Some(true);
                }
                if d.kind != "alert" && ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
    });
    match answer {
        Some(a) => {
            inst.answer_dialog(a, &text);
            ui.data_mut(|m| {
                m.remove::<String>(text_id);
            });
        }
        None => ui.data_mut(|m| {
            m.insert_temp(text_id, text);
        }),
    }
}

/// A `<select>`'s options, under it (`k` points per CSS pixel).
fn select_view(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, rect: Rect, k: f32, id: Id, s: &OpenSelect) {
    let [x, y, w, h] = s.select.rect;
    let (x, y, w, h) = (x * k, y * k, w * k, h * k);
    let at = rect.min + vec2(x, y + h);
    let mut picked = None;
    let area = egui::Area::new(id.with("area")).order(egui::Order::Tooltip).fixed_pos(at).show(ui.ctx(), |ui| {
        egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).inner_margin(Margin::same(4)).show(ui, |ui| {
            ui.set_min_width(w.max(120.0));
            ScrollArea::vertical().max_height((rect.bottom() - at.y - 10.0).max(120.0)).show(ui, |ui| {
                for (i, o) in s.select.options.iter().enumerate() {
                    if ui.selectable_label(s.select.selected == Some(i), o).clicked() {
                        picked = Some(i);
                    }
                }
            });
        });
    });
    let escape = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    if let Some(i) = picked {
        inst.pick_option(s.at, i);
    }
    // The release of the press that opened it, or any click on the select
    // itself, leaves it open.
    let sel_rect = Rect::from_min_size(rect.min + vec2(x, y), vec2(w, h));
    let on_select = ui.input(|i| i.pointer.press_origin()).is_some_and(|o| sel_rect.contains(o));
    if picked.is_some() || escape || (area.response.clicked_elsewhere() && !on_select) {
        ui.data_mut(|d| d.remove::<OpenSelect>(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{Headless, click, conv_json, key, run_frame};

    #[test]
    fn windowed_protocols_select_the_shown_subagent() {
        let mut conv = conv_json("windowed-subagent", "Agent Blender", serde_json::json!([]));
        conv["parent"] = serde_json::json!("parent-agent");
        conv["agent_id"] = serde_json::json!(7);
        let (_dir, _h, mut app) = Headless::start("windowed-subagent", &[conv]);
        app.page = Page::Chats;
        app.screen = Screen::Chat("windowed-subagent".into());
        assert_eq!(app.program_conv().as_deref(), Some("windowed-subagent"));
        assert!(app.browser_conv().is_none(), "browser restrictions stay separate");
        app.screen = Screen::Chat("missing-agent".into());
        assert!(app.program_conv().is_none());
    }

    /// A screencast can arrive at CSS resolution even on a HiDPI screen.
    /// Its image must fill the same rectangle that receives page input.
    #[test]
    fn page_texture_fills_viewport_at_every_screen_scale() {
        let rect = Rect::from_min_size(egui::pos2(700.0, 40.0), vec2(600.0, 900.0));
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for frame_scale in [1.0, scale] {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(scale);
                let size = [(rect.width() * frame_scale) as usize, (rect.height() * frame_scale) as usize];
                let texture = ctx.load_texture("page-test", egui::ColorImage::filled(size, Color32::WHITE), egui::TextureOptions::LINEAR);
                let out = run_frame(&ctx, vec2(1400.0, 1000.0), &mut 0.0, 1.0 / 60.0, vec![], |ui| paint_page_texture(ui.painter(), &texture, rect));
                let meshes = ctx.tessellate(out.shapes, out.pixels_per_point);
                let mesh = meshes.iter().find_map(|p| match &p.primitive {
                    egui::epaint::Primitive::Mesh(m) if m.texture_id == texture.id() => Some(m),
                    _ => None,
                }).expect("page image was not painted");
                assert_eq!(mesh.calc_bounds(), rect, "screen scale {scale}, frame scale {frame_scale}");
            }
        }
    }

    /// The preview sits in the room between the column and the minimap
    /// when there is some, in the viewport's shape, and over the
    /// transcript's corner when there isn't; never past either edge.
    #[test]
    fn preview_fits_beside_the_column_or_over_it() {
        let size = (DEFAULT_W_TEST, DEFAULT_H_TEST);
        for (w, h) in [(1920.0, 1000.0), (1500.0, 900.0), (1280.0, 720.0), (1000.0, 700.0), (700.0, 500.0), (2560.0, 1400.0)] {
            let view = Rect::from_min_size(egui::pos2(0.0, 66.0), vec2(w, h - 66.0 - 120.0));
            let f = chat_view::Frame::new(view);
            let right = f.map.map_or(f.track.left(), |m| m.left());
            let (r, over) = preview_layout(view, f.col, right, view.top() + 12.0, size);
            assert!(r.right() <= right - 13.0 && r.top() >= view.top(), "{w}x{h}: {r:?}");
            assert!(((r.width() / r.height()) - 16.0 / 9.0).abs() < 0.02, "{w}x{h}: keeps the viewport's shape");
            if over {
                assert!(right - 14.0 - (f.col.max + 20.0) < PREVIEW_MIN_W, "{w}x{h}: over the column though there was room");
                assert!(r.left() >= view.left(), "{w}x{h}");
            } else {
                assert!(r.left() >= f.col.max + 19.0, "{w}x{h}: overlaps the column: {r:?} {:?}", f.col);
                assert!(r.width() <= PREVIEW_MAX_W + 0.5, "{w}x{h}");
            }
        }
        // A wide window has the room; a narrow one doesn't.
        let wide = Rect::from_min_size(egui::pos2(0.0, 66.0), vec2(1920.0, 800.0));
        let f = chat_view::Frame::new(wide);
        assert!(!preview_layout(wide, f.col, f.map.unwrap().left(), 78.0, size).1);
        // A tall phone viewport is narrow and tall, at most its height.
        let (r, _) = preview_layout(wide, f.col, f.map.unwrap().left(), 78.0, (390, 844));
        assert!(r.height() <= 300.0 && r.width() < r.height());
    }

    /// The lightbox shows the whole viewport, in its shape, inside the
    /// window, never above 1:1, at every window size.
    #[test]
    fn lightbox_fits_the_window() {
        for (w, h) in [(1920.0, 1080.0), (1280.0, 720.0), (800.0, 600.0), (3840.0, 2160.0), (600.0, 1000.0)] {
            let area = Rect::from_min_max(egui::pos2(0.0, TITLE_H), egui::pos2(w, h));
            for size in [(1920, 1080), (390, 844), (1280, 800), (4096, 200)] {
                let (frame, page, k) = lightbox_layout(area, size);
                assert!(area.contains_rect(frame), "{w}x{h} {size:?}: {frame:?}");
                assert!(page.bottom() + CAPTION_H <= area.bottom(), "{w}x{h} {size:?}: room for the caption");
                assert!(k <= 1.0);
                assert!((page.width() - size.0 as f32 * k).abs() < 0.01 && (page.height() - size.1 as f32 * k).abs() < 0.01);
                // As large as it can be: one side fills its room (or it is 1:1).
                let room = area.shrink2(vec2((area.width() * 0.03).max(24.0), (area.height() * 0.03).max(20.0)));
                let fills = (page.width() - room.width()).abs() < 1.0 || (page.height() - (room.height() - BAR_H - CAPTION_H)).abs() < 1.0;
                assert!(fills || k == 1.0, "{w}x{h} {size:?}: {page:?} in {room:?}");
            }
        }
    }

    /// The model's cursor glides from where it was to where it goes and
    /// stays there.
    #[test]
    fn agent_pointer_glides_and_arrives() {
        let ptr = browser::AgentPointer { from: egui::pos2(0.0, 0.0), to: egui::pos2(100.0, 50.0), at: std::time::Instant::now(), glide: std::time::Duration::from_millis(200) };
        let (start, moving) = ptr.now();
        assert!(moving && start.x < 30.0);
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert_eq!(ptr.now(), (egui::pos2(100.0, 50.0), false));
    }

    const DEFAULT_W_TEST: u32 = browser::DEFAULT_W;
    const DEFAULT_H_TEST: u32 = browser::DEFAULT_H;

    /// A conversation without a browser asks for no more frames: the
    /// browser's fade-in once had the window drawn again every frame while
    /// nothing changed (over two cores' worth of CPU on a software
    /// renderer).
    #[test]
    fn no_browser_no_frames() {
        let ctx = egui::Context::default();
        for i in 0..5 {
            let input = egui::RawInput { time: Some(i as f64 / 60.0), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| drop(tick(ui.ctx(), "no-browser-test")));
            out.textures_delta.clear();
            let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay).unwrap();
            assert!(i < 2 || delay > std::time::Duration::from_secs(1), "frame {i} asked for another in {delay:?}: {:?}", ctx.repaint_causes());
        }
    }

    /// The whole app with a browser the model opened: it fades in as a
    /// preview between the transcript's column and the minimap; a click
    /// expands it, and a click on the page is the page's; Ctrl+Shift+B
    /// takes it back to the preview; closed, it fades out. Starts a real
    /// browser.
    #[test]
    #[ignore]
    fn whole_app_preview_expands_and_fades() {
        const ID: &str = "preview-test";
        let messages = serde_json::json!([{"role":"user","content":[{"type":"text","text":"hi"}]}]);
        let (_dir, mut h, mut app) = Headless::start("browser-preview", &[conv_json(ID, "T", messages)]);
        h.screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1920.0, 1000.0));
        app.open(ID.into());
        h.events(&mut app, vec![]);
        let inst = browser::open(ID, &h.ctx);
        let preview = |h: &Headless| h.ctx.read_response(state_id(ID).with("preview")).map(|r| r.rect);
        let alpha = |h: &Headless| h.ctx.data(|d| d.get_temp::<ViewState>(state_id(ID))).map_or(0.0, |s| s.alpha);
        h.events(&mut app, vec![]);
        assert!(preview(&h).is_some(), "no preview");
        assert!(alpha(&h) < 1.0, "it fades in");
        let start = std::time::Instant::now();
        while inst.view.lock().unwrap().status != Status::Ready || alpha(&h) < 1.0 {
            assert!(start.elapsed().as_secs() < 30, "the browser didn't start");
            std::thread::sleep(std::time::Duration::from_millis(50));
            h.events(&mut app, vec![]);
        }
        // Beside the column, left of the minimap, near the top.
        let r = preview(&h).unwrap();
        let view = h.ctx.data(|d| d.get_temp::<ViewState>(state_id(ID))).unwrap().beside.unwrap();
        let f = chat_view::Frame::new(view);
        assert!(r.left() > f.col.max && r.right() < f.map.unwrap().left(), "{r:?} {:?} {:?}", f.col, f.map);
        assert!(r.top() < view.top() + 20.0);
        // A click expands it; the page then takes up most of the window.
        for e in click(r.center(), egui::PointerButton::Primary) {
            h.events(&mut app, vec![e]);
        }
        for _ in 0..30 {
            h.events(&mut app, vec![]);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let page = h.ctx.read_response(page_id(ID)).expect("no page").rect;
        assert!(page.width() > 1500.0, "{page:?}");
        for e in click(page.center(), egui::PointerButton::Primary) {
            h.events(&mut app, vec![e]);
        }
        assert!(h.ctx.memory(|m| m.has_focus(page_id(ID))));
        // Ctrl+Shift+B: back to the preview, the page without the focus.
        h.events(&mut app, vec![key(Key::B, Modifiers::COMMAND | Modifiers::SHIFT)]);
        assert!(!h.ctx.memory(|m| m.has_focus(page_id(ID))));
        assert!(!h.ctx.data(|d| d.get_temp::<ViewState>(state_id(ID))).unwrap().expanded);
        // Closed: it fades out, then goes.
        browser::close(ID);
        h.events(&mut app, vec![]);
        assert!(alpha(&h) > 0.0);
        let start = std::time::Instant::now();
        while alpha(&h) > 0.0 {
            assert!(start.elapsed().as_secs() < 2, "it didn't fade out");
            std::thread::sleep(std::time::Duration::from_millis(20));
            h.events(&mut app, vec![]);
        }
    }
}
