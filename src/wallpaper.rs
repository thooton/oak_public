//! Home's wallpaper: Oak's forest, or a picture from the `wallpapers`
//! folder in Oak's folder (PNG, JPEG, GIF or WebP; an animated GIF or WebP
//! plays, looping), picked on Settings.
//!
//! A `Player` decodes its file's frames on a thread of its own, two ahead
//! of the screen, and Home shows each when its time comes, reusing one
//! texture. While Home isn't shown the thread waits on the full queue,
//! doing nothing; it ends with the player (another wallpaper picked) or
//! after a still picture's one frame.

use eframe::egui;
use image::AnimationDecoder;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

/// The files the folder's list offers.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
/// A picture this wide or narrower is drawn pixelated when scaled up
/// (pixel art); a wider one smooth.
const PIXEL_ART_W: usize = 1280;
/// The shortest time a frame stays (a GIF may say 0).
const MIN_DELAY: Duration = Duration::from_millis(20);

pub(crate) fn folder() -> PathBuf {
    crate::store::dir().join("wallpapers")
}

/// Wallpaper files: (file name, name shown).
type Files = Vec<(String, String)>;

/// The folder's pictures, by name. Read again when the last look is older
/// than three seconds.
pub(crate) fn list() -> Files {
    static SEEN: Mutex<Option<(Instant, Files)>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap();
    if let Some((at, list)) = &*seen
        && at.elapsed() < Duration::from_secs(3)
    {
        return list.clone();
    }
    let mut list: Files = std::fs::read_dir(folder())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let ext = extension(Path::new(&file));
            EXTENSIONS.contains(&ext.as_str()).then(|| (file.clone(), display_name(&file)))
        })
        .collect();
    list.sort_by_key(|a| a.1.to_lowercase());
    *seen = Some((Instant::now(), list.clone()));
    list
}

/// A file's name as the list shows it: "twilight-forest.webp" is
/// "Twilight forest".
pub(crate) fn display_name(file: &str) -> String {
    let stem = Path::new(file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let words = stem.replace(['-', '_'], " ");
    let mut c = words.trim().chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => file.to_string(),
    }
}

fn extension(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

struct Frame {
    image: egui::ColorImage,
    delay: Duration,
}

/// A wallpaper file playing.
pub(crate) struct Player {
    /// Its file's name in the folder.
    pub(crate) file: String,
    frames: Receiver<Frame>,
    texture: Option<egui::TextureHandle>,
    /// When the frame shown has been up long enough.
    next: Instant,
    /// No more frames will come (a still picture, or one that couldn't be read).
    done: bool,
}

impl Player {
    pub(crate) fn new(file: &str) -> Player {
        let (tx, rx) = sync_channel(2);
        let path = folder().join(file);
        let _ = std::thread::Builder::new().name("wallpaper".into()).spawn(move || decode(&path, &tx));
        Player { file: file.to_string(), frames: rx, texture: None, next: Instant::now(), done: false }
    }

    /// The frame to show now, and asks for the frame that shows the next.
    /// None until the first frame is decoded, and for good when the file
    /// can't be read (Home then shows the forest).
    pub(crate) fn frame(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        let now = Instant::now();
        while !self.done && now >= self.next {
            match self.frames.try_recv() {
                Ok(f) => {
                    let options = if f.image.size[0] <= PIXEL_ART_W { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR };
                    match &mut self.texture {
                        Some(t) => t.set(f.image, options),
                        None => self.texture = Some(ctx.load_texture("wallpaper", f.image, options)),
                    }
                    // Keeps to the file's timing; after a wait (Home
                    // wasn't shown) it starts again from now.
                    self.next = self.next.max(now - Duration::from_millis(250)) + f.delay;
                }
                Err(TryRecvError::Empty) => {
                    // The decoder is behind: look again shortly.
                    ctx.request_repaint_after(Duration::from_millis(10));
                    break;
                }
                Err(TryRecvError::Disconnected) => self.done = true,
            }
        }
        if !self.done {
            ctx.request_repaint_after(self.next.saturating_duration_since(now));
        }
        self.texture.as_ref()
    }
}

/// Decodes `path`'s frames into `out`, over and over, until the player is
/// gone; a still picture's one frame, once.
fn decode(path: &Path, out: &SyncSender<Frame>) {
    let Ok(data) = std::fs::read(path) else { return };
    let ext = extension(path);
    loop {
        let mut sent = 0;
        let mut send = |rgba: image::RgbaImage, delay: Duration| {
            let size = [rgba.width() as usize, rgba.height() as usize];
            let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            sent += 1;
            out.send(Frame { image, delay: delay.max(MIN_DELAY) }).is_ok()
        };
        let frames = match ext.as_str() {
            "gif" => image::codecs::gif::GifDecoder::new(Cursor::new(&data[..])).map(|d| Some(d.into_frames())),
            "webp" => image::codecs::webp::WebPDecoder::new(Cursor::new(&data[..])).map(|d| d.has_animation().then(|| d.into_frames())),
            _ => Ok(None),
        };
        match frames {
            Ok(Some(frames)) => {
                for f in frames {
                    let Ok(f) = f else { break };
                    let (num, den) = f.delay().numer_denom_ms();
                    let delay = Duration::from_micros(num as u64 * 1000 / den.max(1) as u64);
                    if !send(f.into_buffer(), delay) {
                        return;
                    }
                }
            }
            Ok(None) => {
                if let Ok(img) = image::load_from_memory(&data) {
                    send(img.to_rgba8(), Duration::ZERO);
                }
                return;
            }
            Err(_) => return,
        }
        // One frame (or none) is a still picture: nothing to loop.
        if sent <= 1 {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_files() {
        assert_eq!(display_name("twilight-forest.webp"), "Twilight forest");
        assert_eq!(display_name("rainy_night cabins.gif"), "Rainy night cabins");
        assert_eq!(display_name("x.png"), "X");
    }

    /// An animated GIF plays its frames in turn and loops; a still PNG
    /// gives one frame and the thread ends.
    #[test]
    fn frames_play_and_loop() {
        let dir = std::env::temp_dir().join(format!("oak-wallpaper-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gif = dir.join("two.gif");
        {
            let mut f = std::fs::File::create(&gif).unwrap();
            let mut enc = image::codecs::gif::GifEncoder::new(&mut f);
            enc.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
            for c in [[255u8, 0, 0, 255], [0, 0, 255, 255]] {
                let img = image::RgbaImage::from_pixel(4, 2, image::Rgba(c));
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(50, 1))).unwrap();
            }
        }
        let (tx, rx) = sync_channel(8);
        std::thread::spawn(move || decode(&gif, &tx));
        let firsts: Vec<u8> = (0..5).map(|_| rx.recv().unwrap()).map(|f| f.image.pixels[0].r()).collect();
        assert_eq!(firsts, vec![255, 0, 255, 0, 255]);
        drop(rx);

        let png = dir.join("still.png");
        image::RgbaImage::from_pixel(3, 3, image::Rgba([1, 2, 3, 255])).save(&png).unwrap();
        let (tx, rx) = sync_channel(8);
        decode(&png, &tx);
        drop(tx);
        assert_eq!(rx.iter().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
