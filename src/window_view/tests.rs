//! Regression tests for frame identity across independent windowed protocols.
use super::*;

#[test]
fn independent_windows_keep_independent_textures() {
    let ctx = egui::Context::default();
    let (a, b) = (usize::MAX - 1, usize::MAX - 2);
    let mut frames = FRAMES.lock().unwrap();
    for (w, color, client) in [(a, [255, 0, 0, 255], (100, 50)), (b, [0, 0, 255, 255], (200, 90))] {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
        frames.insert(w, Slot { until: Instant::now() + WANT_FOR, expanded: false, ctx: ctx.clone(), frame: Some(Frame { seq: 1, window: w, img: Arc::new(img), client }) });
    }
    drop(frames);
    let (ta, ca) = texture(&ctx, a).unwrap();
    let (tb, cb) = texture(&ctx, b).unwrap();
    assert_ne!(ta.id(), tb.id(), "same sequence number must not alias another window's texture");
    assert_eq!(ca, (100, 50));
    assert_eq!(cb, (200, 90));
    assert_eq!(texture(&ctx, a).unwrap().0.id(), ta.id());
    assert!(texture(&ctx, usize::MAX).is_none());
    let mut frames = FRAMES.lock().unwrap();
    frames.remove(&a);
    frames.remove(&b);
}
