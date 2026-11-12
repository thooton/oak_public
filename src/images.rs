// Images: attachments for messages (pasted, dropped or picked), image files
// opened with READ, and drawing them in the UI. The API takes PNG, JPEG,
// GIF and WebP up to 5 MB, and scales anything over 1568 px on its long
// edge down itself (slower, same cost); OAK does that scaling up front, so
// a 4K screenshot isn't sent and stored at full size.

use base64::Engine;
use eframe::egui::{self, ColorImage, TextureHandle, Ui};
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;

/// Longest edge the API uses without scaling.
const MAX_EDGE: u32 = 1568;
/// Leaves room under the API's 5 MB limit.
const MAX_BYTES: usize = 3_750_000;

/// An image ready to send: an Oak image block's media type and data.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub media_type: &'static str,
    pub data: String,
    pub width: u32,
    pub height: u32,
}

impl Image {
    /// The content block for a message or a tool result.
    pub fn block(&self) -> Value {
        crate::oak::image(self.media_type, &self.data)
    }
}

fn media_type(f: ImageFormat) -> Option<&'static str> {
    Some(match f {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return None,
    })
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Encodes a decoded image: PNG (lossless, right for screenshots), or JPEG
/// when the source was a JPEG or the PNG comes out too big.
fn encode(img: &DynamicImage, prefer_jpeg: bool) -> Result<Image, String> {
    let (width, height) = (img.width(), img.height());
    let jpeg = |img: &DynamicImage| -> Result<Vec<u8>, String> {
        let mut out = vec![];
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        img.to_rgb8().write_with_encoder(enc).map_err(|e| e.to_string())?;
        Ok(out)
    };
    if !prefer_jpeg {
        let mut png = Cursor::new(vec![]);
        img.write_to(&mut png, ImageFormat::Png).map_err(|e| e.to_string())?;
        let png = png.into_inner();
        if png.len() <= MAX_BYTES {
            return Ok(Image { media_type: "image/png", data: b64(&png), width, height });
        }
    }
    Ok(Image { media_type: "image/jpeg", data: b64(&jpeg(img)?), width, height })
}

fn shrink(img: DynamicImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w.max(h) <= MAX_EDGE {
        return img;
    }
    let scale = MAX_EDGE as f64 / w.max(h) as f64;
    img.resize(((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1), FilterType::Lanczos3)
}

/// An image file's bytes, ready to send. Kept as they are when the API
/// takes them as they are; otherwise scaled down and re-encoded.
pub fn from_bytes(bytes: &[u8]) -> Result<Image, String> {
    let format = image::guess_format(bytes).map_err(|_| "not an image Oak can read".to_string())?;
    let img = image::load_from_memory_with_format(bytes, format).map_err(|e| format!("couldn't read the image: {e}"))?;
    if let Some(mt) = media_type(format)
        && img.width().max(img.height()) <= MAX_EDGE
        && bytes.len() <= MAX_BYTES
    {
        return Ok(Image { media_type: mt, data: b64(bytes), width: img.width(), height: img.height() });
    }
    encode(&shrink(img), format == ImageFormat::Jpeg)
}

/// A pasted image (from the clipboard, already decoded).
pub fn from_color_image(c: &ColorImage) -> Result<Image, String> {
    let [w, h] = c.size;
    let rgba = image::RgbaImage::from_raw(w as u32, h as u32, c.as_raw().to_vec()).ok_or("bad image data")?;
    encode(&shrink(DynamicImage::ImageRgba8(rgba)), false)
}

pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"))
}

/// READ on an image file: the image itself, for the model to look at.
pub fn read_for_model(path: &Path) -> Result<Vec<Value>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let img = from_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let note = format!("Image {} ({}x{} as sent, {} bytes on disk)", path.display(), img.width, img.height, bytes.len());
    Ok(vec![json!({"type": "text", "text": note}), img.block()])
}

// ---------------------------------------------------------------- drawing

/// How long a texture is kept after it was last drawn. Opening a
/// conversation measures every part of it, pictures too, so only their
/// sizes are read then; the pixels are decoded once one is on screen, and
/// let go again once it hasn't been for this long.
const KEEP_UNSEEN: f64 = 60.0;

/// An image block's size in pixels (None: it can't be read), and its
/// texture while it is being drawn.
struct Cached {
    size: Option<[usize; 2]>,
    tex: Option<TextureHandle>,
    /// When it was last drawn (egui's time, in seconds).
    used: f64,
}

thread_local! {
    /// By the hash of the block's data.
    static CACHE: RefCell<HashMap<u64, Cached>> = RefCell::new(HashMap::new());
}

fn key(data: &str) -> u64 {
    crate::hash_of(data)
}

/// The size of the image in base64 `data`, from its header alone.
fn read_size(data: &str) -> Option<[usize; 2]> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let (w, h) = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()?;
    Some([w as usize, h as usize])
}

fn decode(ctx: &egui::Context, key: u64, data: &str) -> Option<TextureHandle> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture(format!("image-{key}"), color, egui::TextureOptions::LINEAR))
}

/// The size of the image in base64 `data` (hashed to `key`), read once.
fn size(key: u64, data: &str) -> Option<[usize; 2]> {
    CACHE.with(|c| c.borrow_mut().entry(key).or_insert_with(|| Cached { size: read_size(data), tex: None, used: 0.0 }).size)
}

/// The texture of the image in base64 `data` (hashed to `key`), decoded
/// the first time it is drawn (None if it can't be).
fn texture(ctx: &egui::Context, key: u64, data: &str) -> Option<TextureHandle> {
    let now = ctx.input(|i| i.time);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let e = c.entry(key).or_insert_with(|| Cached { size: read_size(data), tex: None, used: now });
        e.used = now;
        if e.tex.is_none() && e.size.is_some() {
            e.tex = decode(ctx, key, data);
            // What can't be decoded isn't shown, whatever its header says.
            e.size = e.tex.as_ref().map(|t| t.size());
        }
        e.tex.clone()
    })
}

thread_local! {
    /// A clear pixel, drawn in place of a picture that isn't on screen.
    static CLEAR: RefCell<Option<TextureHandle>> = const { RefCell::new(None) };
}

fn placeholder(ctx: &egui::Context) -> TextureHandle {
    CLEAR.with(|c| c.borrow_mut().get_or_insert_with(|| ctx.load_texture("image-placeholder", ColorImage::filled([1, 1], egui::Color32::TRANSPARENT), egui::TextureOptions::LINEAR)).clone())
}

thread_local! {
    /// When the last sweep ran (`forget_unseen`).
    static SWEPT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// Lets go of the textures not drawn for `KEEP_UNSEEN` (their sizes stay).
/// Called every frame; does the work every few seconds.
pub fn forget_unseen(ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    if now - SWEPT.get() < 5.0 {
        return;
    }
    SWEPT.set(now);
    CACHE.with(|c| {
        for e in c.borrow_mut().values_mut() {
            if e.tex.is_some() && now - e.used > KEEP_UNSEEN {
                e.tex = None;
            }
        }
    });
}

/// Draws an image block at most `max_h` tall (and as wide as there is
/// room). Clicking it opens it in the in-app viewer (`viewer`). Its pixels
/// are only decoded when it is on screen.
pub fn show(ui: &mut Ui, block: &Value, max_h: f32) -> Option<egui::Response> {
    let data = block["data"].as_str().unwrap_or("");
    let key = key(data);
    let Some([w, h]) = size(key, data).map(|s| s.map(|v| v as f32)) else {
        ui.label(egui::RichText::new("[image that couldn't be shown]").small());
        return None;
    };
    let scale = (max_h / h).min(ui.available_width() / w).min(1.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w * scale, h * scale), egui::Sense::click());
    let seen = ui.is_rect_visible(rect) && ui.ctx().content_rect().intersects(rect);
    if (seen || resp.clicked())
        && let Some(tex) = texture(ui.ctx(), key, data)
    {
        egui::Image::new(&tex).paint_at(ui, rect);
        if resp.clicked() {
            open_viewer(ui.ctx(), block, tex);
        }
    } else if !seen {
        // Off screen (measured for the list, or clipped): a clear stand-in
        // in its place, so the minimap still draws a picture there.
        egui::Image::new(&placeholder(ui.ctx())).paint_at(ui, rect);
    }
    let resp = resp.on_hover_text(format!("{}×{}, click to view", w as u32, h as u32));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    Some(resp)
}

// ------------------------------------------------------------ the viewer
//
// A clicked image opens over everything, in a frame well inside the
// window, with the rest dimmed. Zooming works as the Zoom mods of
// Geometry Dash do it: each wheel notch scales by 1.1 around the pointer
// (the image point under it stays put), never below the fitted size, and
// the view is clamped so the image always fills the frame. The frame
// itself never changes size: zooming happens inside it. Dragging with the
// right mouse button pans.

/// The open viewer: the image, how far it is zoomed (1 = fitted to the
/// frame) and where its center is, relative to the frame's center.
#[derive(Clone)]
struct Viewer {
    tex: TextureHandle,
    block: Value,
    zoom: f32,
    offset: egui::Vec2,
}

fn viewer_id(ctx: &egui::Context) -> egui::Id {
    egui::Id::unique(("image-viewer", ctx.viewport_id()))
}

/// The scale step of one wheel notch, as in the Zoom mod.
const ZOOM_STEP: f32 = 1.1;
/// A wheel notch, in points of scroll (egui's line scroll speed).
const NOTCH: f32 = 40.0;

/// The closest zoom: one image pixel at most this many screen pixels.
const MAX_PIXEL: f32 = 32.0;
/// How far a click (or, zooming with the wheel, a double click) zooms in.
const CLICK_ZOOM: f32 = 2.5;

/// How the viewer zooms (Settings, IMAGE ZOOM).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomMode {
    /// The wheel zooms around the pointer (the Zoom mod's way).
    Scroll,
    /// A left click zooms in a set step, the next back out; a left drag
    /// pans (Claude Code's way).
    Click,
}

fn open_viewer(ctx: &egui::Context, block: &Value, tex: TextureHandle) {
    let id = viewer_id(ctx);
    ctx.data_mut(|d| d.insert_temp(id, Viewer { tex, block: block.clone(), zoom: 1.0, offset: egui::Vec2::ZERO }));
}

/// Keeps the zoomed image covering the frame (no empty edges).
fn clamp_offset(offset: egui::Vec2, frame: egui::Vec2, zoom: f32) -> egui::Vec2 {
    let limit = (frame * zoom - frame) * 0.5;
    egui::vec2(offset.x.clamp(-limit.x, limit.x), offset.y.clamp(-limit.y, limit.y))
}

/// Zooms by `factor` around `anchor` (relative to the frame's center), so
/// the image point under it stays where it is; the zoom stays within
/// 1..=`max`. Returns the new zoom and offset.
fn zoom_at(zoom: f32, offset: egui::Vec2, anchor: egui::Vec2, factor: f32, max: f32, frame: egui::Vec2) -> (f32, egui::Vec2) {
    let new = (zoom * factor).clamp(1.0, max.max(1.0));
    let offset = anchor + (offset - anchor) * (new / zoom);
    (new, clamp_offset(offset, frame, new))
}

/// The viewer's colors, from the app's theme.
pub struct ViewerStyle {
    pub ink: egui::Color32,
    pub ink3: egui::Color32,
    pub line: egui::Color32,
    pub panel: egui::Color32,
}

/// Draws the viewer when an image is open, over everything below `top`
/// (the window's title bar stays usable). Escape, the X or a click beside
/// the image closes it. Call it before the rest of the frame's UI, so it
/// gets the Escape key first.
pub fn viewer(ctx: &egui::Context, top: f32, style: &ViewerStyle, mode: ZoomMode) {
    let id = viewer_id(ctx);
    let Some(mut v) = ctx.data(|d| d.get_temp::<Viewer>(id)) else { return };
    let mut close = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    let screen = ctx.content_rect();
    let area = egui::Rect::from_min_max(egui::pos2(screen.left(), screen.top() + top), screen.max);
    let ppp = ctx.pixels_per_point();
    let [iw, ih] = v.tex.size().map(|s| s as f32);
    let margin = egui::vec2((area.width() * 0.08).max(48.0), (area.height() * 0.08).max(48.0));
    let mut room = area.shrink2(margin);
    room.max.y -= 24.0; // room for the caption
    let native = egui::vec2(iw, ih) / ppp;
    let frame = egui::Rect::from_center_size(room.center(), native * (room.width() / native.x).min(room.height() / native.y).clamp(0.0, 1.0));
    let max_zoom = MAX_PIXEL * iw / (frame.width() * ppp);
    egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(area.min).show(ctx, |ui| {
        let (rect, resp) = ui.allocate_exact_size(area.size(), egui::Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(200));
        close |= v.steer(ui, &resp, frame, max_zoom, mode);
        p.rect_filled(frame, 0.0, style.panel);
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        p.with_clip_rect(frame).image(v.tex.id(), egui::Rect::from_center_size(frame.center() + v.offset, frame.size() * v.zoom), uv, egui::Color32::WHITE);
        p.rect_stroke(frame, 0.0, egui::Stroke::new(1.0, style.line), egui::StrokeKind::Outside);
        let how = match mode {
            ZoomMode::Scroll => "scroll to zoom, right-drag to pan, double-click to zoom",
            ZoomMode::Click => "click to zoom, drag to pan",
        };
        let caption = format!("{}×{}, {:.0}%, {how}", iw as u32, ih as u32, frame.width() * v.zoom / iw * ppp * 100.0);
        caption_row(ui, frame, caption, style, &v.block);
        close |= close_button(ui, rect, style);
    });
    ctx.data_mut(|d| {
        if close {
            d.remove::<Viewer>(id);
        } else {
            d.insert_temp(id, v);
        }
    });
}

impl Viewer {
    /// Zooms and pans the image shown in `frame` as the pointer says
    /// (`resp` is the whole viewer's), setting the cursor to match. True
    /// when a click beside the image asks to close.
    fn steer(&mut self, ui: &Ui, resp: &egui::Response, frame: egui::Rect, max_zoom: f32, mode: ZoomMode) -> bool {
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let over_image = pointer.is_some_and(|p| frame.contains(p));
        if mode == ZoomMode::Scroll {
            // Zoom with the wheel (or a pinch / Ctrl+wheel) around the pointer.
            let factor = wheel_factor(ui);
            if factor != 1.0 {
                let at = pointer.map(|p| frame.clamp(p)).unwrap_or(frame.center());
                (self.zoom, self.offset) = zoom_at(self.zoom, self.offset, at - frame.center(), factor, max_zoom, frame.size());
            }
        }
        // Pan 1:1 with the pointer: with the right mouse button when the
        // wheel zooms; when clicks do, as in Claude Code, everything is the
        // left one (a drag pans, a click without one toggles the zoom).
        let pan_button = match mode {
            ZoomMode::Scroll => egui::PointerButton::Secondary,
            ZoomMode::Click => egui::PointerButton::Primary,
        };
        if resp.dragged_by(pan_button) {
            self.offset = clamp_offset(self.offset + resp.drag_delta(), frame.size(), self.zoom);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if over_image && mode == ZoomMode::Click {
            ui.ctx().set_cursor_icon(if self.zoom > 1.0 { egui::CursorIcon::ZoomOut } else { egui::CursorIcon::ZoomIn });
        } else if over_image && self.zoom > 1.0 {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        let on_image = resp.interact_pointer_pos().is_some_and(|p| frame.contains(p));
        let toggle = match mode {
            // A click on the image zooms in a set step there, the next back out.
            ZoomMode::Click => resp.clicked() && on_image,
            // A double click on the image zooms in there, or back out.
            ZoomMode::Scroll => resp.double_clicked() && on_image,
        };
        if !toggle {
            return resp.clicked() && !on_image;
        }
        let anchor = resp.interact_pointer_pos().unwrap() - frame.center();
        let factor = if self.zoom > 1.0 { 1.0 / self.zoom } else { CLICK_ZOOM };
        (self.zoom, self.offset) = zoom_at(self.zoom, self.offset, anchor, factor, max_zoom, frame.size());
        false
    }
}

/// This frame's wheel notches and pinch as a zoom factor (Ctrl+wheel is
/// egui's own zoom, so it comes in as the pinch).
fn wheel_factor(ui: &Ui) -> f32 {
    let (notches, pinch) = ui.input(|i| (i.raw.events.iter().map(|e| match e {
        egui::Event::MouseWheel { unit, delta, modifiers, .. } if !modifiers.command => match unit {
            egui::MouseWheelUnit::Line => delta.y,
            egui::MouseWheelUnit::Point => delta.y / NOTCH,
            egui::MouseWheelUnit::Page => delta.y * 3.0,
        },
        _ => 0.0,
    }).sum::<f32>(), i.zoom_delta()));
    ZOOM_STEP.powf(notches) * pinch
}

/// The caption under the image in `frame` (size, zoom and how to use
/// it), and the button opening the image in the system viewer.
fn caption_row(ui: &Ui, frame: egui::Rect, caption: String, style: &ViewerStyle, block: &Value) {
    let p = ui.painter();
    let font = egui::FontId::proportional(12.0);
    let y = frame.bottom() + 16.0;
    p.text(egui::pos2(frame.left(), y), egui::Align2::LEFT_CENTER, caption, font.clone(), style.ink3);
    let ext = p.layout_no_wrap("Open externally".into(), font.clone(), style.ink3);
    let ext_rect = egui::Rect::from_min_size(egui::pos2(frame.right() - ext.size().x, y - ext.size().y / 2.0), ext.size());
    let ext_resp = ui.interact(ext_rect, viewer_id(ui.ctx()).with("external"), egui::Sense::click());
    let ext_color = if ext_resp.hovered() { style.ink } else { style.ink3 };
    p.text(ext_rect.left_center(), egui::Align2::LEFT_CENTER, "Open externally", font, ext_color);
    if ext_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if ext_resp.on_hover_text("Open in image viewer").clicked() {
        open_externally(block);
    }
}

/// The X in the corner of `rect`; true when clicked.
fn close_button(ui: &Ui, rect: egui::Rect, style: &ViewerStyle) -> bool {
    let p = ui.painter();
    let x_rect = egui::Rect::from_center_size(egui::pos2(rect.right() - 32.0, rect.top() + 32.0), egui::vec2(36.0, 36.0));
    let x_resp = ui.interact(x_rect, viewer_id(ui.ctx()).with("close"), egui::Sense::click());
    let lit = x_resp.hovered();
    p.circle_filled(x_rect.center(), 18.0, if lit { egui::Color32::from_white_alpha(40) } else { egui::Color32::from_white_alpha(16) });
    let (c, s) = (x_rect.center(), 6.5);
    let stroke = egui::Stroke::new(1.8, if lit { style.ink } else { style.ink3.gamma_multiply(1.4) });
    p.line_segment([c + egui::vec2(-s, -s), c + egui::vec2(s, s)], stroke);
    p.line_segment([c + egui::vec2(-s, s), c + egui::vec2(s, -s)], stroke);
    if lit {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    x_resp.on_hover_text("Close (Esc)").clicked()
}

/// Writes the image to a temp file and opens it with the default viewer.
fn open_externally(block: &Value) {
    let Some(bytes) = block["data"].as_str().and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
    else {
        return;
    };
    let ext = block["media_type"].as_str().map(|m| match m { "image/jpeg" => "jpg", "image/gif" => "gif", "image/webp" => "webp", _ => "png" }).unwrap_or("png");
    let path = std::env::temp_dir().join(format!("oak-image-{:x}.{ext}", crate::hash_of(&bytes)));
    if std::fs::write(&path, &bytes).is_ok() {
        let _ = open::that(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255])));
        let mut out = Cursor::new(vec![]);
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn big_images_are_scaled_to_the_api_size() {
        let img = from_bytes(&png(3840, 2160)).unwrap();
        assert_eq!((img.width, img.height), (1568, 882));
        let pasted = ColorImage::from_rgba_unmultiplied([2000, 1000], &vec![200u8; 2000 * 1000 * 4]);
        let img = from_color_image(&pasted).unwrap();
        assert_eq!((img.media_type, img.width, img.height), ("image/png", 1568, 784));
    }
}
