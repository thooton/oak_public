//! Private Blender live views: the shown agent's own window is rendered
//! interactively by `window_view::program_preview`, independently of WINDOW
//! selection. Subagent conversations are eligible too. Only explicit user
//! attachments use the legacy viewport-only worker here. The Protocols page
//! also uses that worker to check the user's add-on availability.

use super::*;
use crate::mcp::blender;
use egui::TextureHandle;
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

/// How often a frame is taken while the view is shown.
const EVERY: Duration = Duration::from_secs(1);
/// How long a want lasts without being renewed (a frame not drawn stops it).
const WANT_FOR: Duration = Duration::from_secs(3);
/// Longer side of a frame for the preview, and for the expanded view.
const SMALL: u32 = 720;
const LARGE: u32 = 1600;
/// Seconds the view takes to fade in and out, and to expand.
const FADE: f32 = 0.25;
const EXPAND: f32 = 0.16;

#[derive(Default)]
struct Shared {
    /// Whether Blender answered the last look (None: not looked yet).
    reach: Option<bool>,
    /// Until when the tab wants to know whether Blender is reachable.
    probe_until: Option<Instant>,
    /// Until when the live view wants frames, and how large.
    frames_until: Option<(Instant, u32)>,
    /// The newest frame, numbered.
    frame: Option<(u64, Arc<image::RgbaImage>)>,
    ctx: Option<egui::Context>,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(Mutex::default);

fn start_worker(ctx: &egui::Context) {
    static ONCE: Once = Once::new();
    SHARED.lock().unwrap().ctx.get_or_insert_with(|| ctx.clone());
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("blender-view".into()).spawn(worker);
    });
}

fn worker() {
    let mut seq = 0u64;
    loop {
        let (probe, frames) = {
            let s = SHARED.lock().unwrap();
            let now = Instant::now();
            (s.probe_until.is_some_and(|u| u > now), s.frames_until.filter(|(u, _)| *u > now).map(|(_, max)| max))
        };
        if !probe && frames.is_none() {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let began = Instant::now();
        let (reach, frame) = match frames {
            Some(max) => match blender::frame_on(crate::mcp::blender_run::USER_PORT, max) {
                Some(img) => (true, Some(img)),
                // Busy with a long script, or not there.
                None => (blender::reachable(), None),
            },
            None => (blender::reachable(), None),
        };
        let ctx = {
            let mut s = SHARED.lock().unwrap();
            let changed = s.reach != Some(reach) || frame.is_some();
            s.reach = Some(reach);
            if let Some(img) = frame {
                seq += 1;
                s.frame = Some((seq, Arc::new(img)));
            }
            if !reach {
                s.frame = None;
            }
            s.ctx.clone().filter(|_| changed)
        };
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
        let period = if frames.is_some() { EVERY } else { Duration::from_secs(2) };
        std::thread::sleep(period.saturating_sub(began.elapsed()));
    }
}

/// Keeps the reachability look going a while (Home's MCP tab calls it as
/// it draws).
pub(crate) fn watch(ctx: &egui::Context) {
    start_worker(ctx);
    SHARED.lock().unwrap().probe_until = Some(Instant::now() + WANT_FOR);
}

/// Whether Blender answered on its add-on's port the last time it was
/// looked at (None before the first look).
pub(crate) fn reachable() -> Option<bool> {
    SHARED.lock().unwrap().reach
}

fn want_frames(ctx: &egui::Context, max: u32) {
    start_worker(ctx);
    SHARED.lock().unwrap().frames_until = Some((Instant::now() + WANT_FOR, max));
}

fn expanded_id(conv: &str) -> Id {
    Id::unique(("blender-view-expanded", conv))
}

/// The newest frame as a texture (uploaded once per frame taken).
fn texture(ctx: &egui::Context) -> Option<(TextureHandle, [usize; 2])> {
    let (seq, img) = SHARED.lock().unwrap().frame.clone()?;
    let id = Id::unique("blender-view-texture");
    if let Some((s, h, size)) = ctx.data(|d| d.get_temp::<(u64, TextureHandle, [usize; 2])>(id))
        && s == seq
    {
        return Some((h, size));
    }
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    let h = ctx.load_texture("blender-view", color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, (seq, h.clone(), size)));
    Some((h, size))
}

impl App {
    /// The conversation shown, if it has Blender's kit loaded.
    fn blender_conv(&self) -> Option<String> {
        let conv = self.program_conv()?;
        let s = self.sessions.get(&conv)?;
        let loaded = s.conv.lock().unwrap().prefix.kits.iter().any(|k| crate::mcp::server_of_kit(k).is_some_and(|c| c.name == blender::NAME));
        (loaded && crate::mcp::enabled(blender::NAME)).then_some(conv)
    }

    /// After the transcript (in `view`) and the browser's preview (`above`,
    /// while shown): Blender's live view, and expanded over the window when
    /// clicked.
    pub(crate) fn blender_preview(&mut self, ctx: &egui::Context, t: &Theme, view: Rect, above: Option<Rect>) -> Option<Rect> {
        let selected = self.blender_conv();
        #[cfg(windows)]
        if let Some(conv) = selected.as_deref() && !crate::mcp::blender_run::users_for(conv) {
            let window = crate::mcp::blender_run::window_for(conv).filter(|&w| crate::winctl::alive(w));
            return self.program_preview(ctx, t, view, above, conv, window, "blender");
        }
        // Only an explicit user attachment uses the viewport-only renderer.
        let conv = selected.filter(|c| crate::mcp::blender_run::users_for(c));
        let key = conv.clone().unwrap_or_default();
        let expanded = conv.is_some() && ctx.data(|d| d.get_temp::<bool>(expanded_id(&key))).unwrap_or(false);
        if conv.is_some() {
            want_frames(ctx, if expanded { LARGE } else { SMALL });
        }
        let tex = texture(ctx);
        let live = conv.is_some() && reachable() == Some(true) && tex.is_some();
        let alpha = ctx.animate_bool_with_time(Id::unique("blender-view-fade"), live, FADE);
        let (tex, size) = tex.filter(|_| alpha > 0.0)?;
        let e = ctx.animate_bool_with_time(Id::unique(("blender-view-expand", key.as_str())), expanded && live, EXPAND);

        let (col, right, top) = crate::browser_view::preview_slot(ctx, &key, view);
        let top = above.map_or(top, |r| r.bottom() + 10.0);
        let (rect, over) = crate::browser_view::preview_layout(view, col, right, top, (size[0] as u32, size[1] as u32));
        let shown = alpha * (1.0 - e);
        let uv = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
        if shown > 0.0 {
            let id = Id::unique(("blender-view", key.as_str()));
            egui::Area::new(id).order(egui::Order::Middle).fixed_pos(rect.min).show(ctx, |ui| {
                let (r, resp) = ui.allocate_exact_size(rect.size(), Sense::click());
                let hover = ui.ctx().animate_bool_with_time(id.with("hover"), resp.hovered(), 0.12);
                let rest = if over { crate::browser_view::REST_ALPHA_OVER } else { crate::browser_view::REST_ALPHA };
                let mut p = ui.painter().clone();
                p.set_opacity(shown * (rest + (1.0 - rest) * hover));
                p.rect_filled(r.translate(vec2(0.0, 3.0)).expand(1.0), 6.0, Color32::from_black_alpha(70));
                p.add(egui::epaint::RectShape::filled(r, 5.0, Color32::WHITE).with_texture(tex.id(), uv));
                p.rect_stroke(r, 5.0, Stroke::new(1.0, if hover > 0.5 { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
                // Which preview this is, in its corner.
                let font = egui::FontId::proportional(11.0);
                let galley = p.layout_no_wrap("Blender".into(), font, Color32::WHITE);
                let tag = Rect::from_min_size(r.left_bottom() + vec2(6.0, -6.0 - galley.size().y - 4.0), galley.size() + vec2(10.0, 4.0));
                p.rect_filled(tag, 4.0, Color32::from_black_alpha(150));
                p.galley(tag.min + vec2(5.0, 2.0), galley, Color32::WHITE);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Blender's 3D viewport, live\nClick to expand").clicked() {
                    ui.ctx().data_mut(|d| d.insert_temp(expanded_id(&key), true));
                }
            });
        }
        if e > 0.0 {
            self.blender_lightbox(ctx, t, &key, &tex, size, rect, e);
        }
        (shown > 0.0).then_some(rect)
    }

    /// The view expanded over the window (`e`: how far, from `from`).
    #[allow(clippy::too_many_arguments)]
    fn blender_lightbox(&mut self, ctx: &egui::Context, t: &Theme, conv: &str, tex: &TextureHandle, size: [usize; 2], from: Rect, e: f32) {
        let screen = ctx.content_rect();
        let close = |ctx: &egui::Context| ctx.data_mut(|d| d.insert_temp(expanded_id(conv), false));
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            close(ctx);
        }
        let room = screen.shrink2(vec2((screen.width() * 0.04).max(24.0), (screen.height() * 0.05).max(28.0)));
        let (w, h) = (size[0].max(1) as f32, size[1].max(1) as f32);
        let k = (room.width() / w).min((room.height() - 28.0) / h);
        let full = Rect::from_center_size(egui::pos2(room.center().x, room.center().y - 14.0), vec2(w * k, h * k));
        let r = Rect::from_min_max(from.min.lerp(full.min, e), from.max.lerp(full.max, e));
        let uv = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
        egui::Area::new(Id::unique(("blender-lightbox", conv))).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let (bg, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let p = ui.painter();
            p.rect_filled(bg, 0.0, Color32::from_black_alpha((170.0 * e) as u8));
            p.rect_filled(r.translate(vec2(0.0, 4.0)).expand(2.0), 8.0, Color32::from_black_alpha((90.0 * e) as u8));
            p.add(egui::epaint::RectShape::filled(r, 6.0, Color32::WHITE).with_texture(tex.id(), uv));
            let caption = "Blender's 3D viewport, live (view only: work in Blender's window)";
            p.text(egui::pos2(r.center().x, r.bottom() + 16.0), egui::Align2::CENTER_CENTER, caption, egui::FontId::proportional(12.5), t.ink2.gamma_multiply(e));
            let x = Rect::from_center_size(r.right_top() + vec2(-18.0, 18.0), vec2(26.0, 26.0));
            let xr = ui.interact(x, ui.scope_id().with("x"), Sense::click());
            p.circle_filled(x.center(), 13.0, Color32::from_black_alpha(if xr.hovered() { 200 } else { 140 }));
            let d = 5.0;
            let s = Stroke::new(1.6, Color32::WHITE);
            p.line_segment([x.center() + vec2(-d, -d), x.center() + vec2(d, d)], s);
            p.line_segment([x.center() + vec2(-d, d), x.center() + vec2(d, -d)], s);
            let on_picture = resp.interact_pointer_pos().is_some_and(|pos| r.contains(pos));
            if xr.on_hover_text("Back to the preview (Esc)").clicked() || (resp.clicked() && !on_picture) {
                close(ui.ctx());
            }
        });
    }
}
