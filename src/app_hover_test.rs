//! Headless harnesses for UI tests: the whole app's frames, and one view's
//! frames with what they painted and copied.

use super::*;

/// A conversation file's JSON: `id`, titled `title`, in /tmp, with
/// `messages`.
pub(crate) fn conv_json(id: &str, title: &str, messages: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"id": id, "title": title, "folder": "/tmp", "model": "m", "effort": "low", "messages": messages, "last_message_at": 0, "next_task_id": 0})
}

/// The whole app's frames, headless, at the size of a real window, each
/// a 20th of a second after the last.
pub(crate) struct Headless {
    pub(crate) ctx: egui::Context,
    frame: eframe::Frame,
    pub(crate) screen: Rect,
    time: f64,
}

impl Headless {
    /// Starts the app in a scratch OAK folder named for `name`, signed in
    /// (with an API key), with the conversations `convs`; and the folder.
    pub(crate) fn start(name: &str, convs: &[serde_json::Value]) -> (std::path::PathBuf, Headless, App) {
        let dir = std::env::temp_dir().join(format!("oak-{name}-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        std::fs::write(dir.join("auth.json"), r#"{"type":"api_key","key":"not-a-real-key"}"#).unwrap();
        for c in convs {
            std::fs::write(store::conversations_dir().join(format!("{}.json", c["id"].as_str().unwrap())), c.to_string()).unwrap();
        }
        unsafe { std::env::set_var("OAK_DIR", &dir) };
        let ctx = egui::Context::default();
        let app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()));
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1384.0, 821.0));
        (dir, Headless { ctx, frame: eframe::Frame::_new_kittest(), screen, time: 0.0 }, app)
    }

    /// Runs the next frame of `input`, at the window's size; its whole
    /// output.
    pub(crate) fn output(&mut self, app: &mut App, input: egui::RawInput) -> egui::FullOutput {
        self.time += 0.05;
        let input = egui::RawInput { screen_rect: Some(self.screen), time: Some(self.time), ..input };
        let mut out = self.ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut self.frame));
        out.textures_delta.clear();
        out
    }

    /// Runs the next frame of `events`; its whole output.
    pub(crate) fn events(&mut self, app: &mut App, events: Vec<egui::Event>) -> egui::FullOutput {
        self.output(app, egui::RawInput { events, ..Default::default() })
    }
}

/// A second theme for the tests, warmer and greyer than Oak's (a plugin
/// may bring one like it), so that they cover more than one palette.
pub(crate) fn warm_theme() -> Theme {
    Theme::from_palette(oak_sdk::theme::Palette {
        bg: 0x141413,
        panel: 0x1e1d1b,
        panel2: 0x2a2926,
        ink: 0xfaf9f5,
        ink2: 0xb0aea5,
        ink3: 0x87867f,
        ink4: 0x5e5d59,
        accent: 0xd97757,
        error: 0xe5735f,
        line_alpha: 0x29,
    })
}

/// A context with the app's fonts and, when `styled`, its style and the
/// theme applied; and the theme (Oak's when `oak`, else `warm_theme`).
pub(crate) fn test_ctx(oak: bool, styled: bool) -> (egui::Context, Theme) {
    let ctx = egui::Context::default();
    markdown::install_fonts(&ctx);
    let t = if oak { Theme::oak() } else { warm_theme() };
    if styled {
        setup_style(&ctx);
        apply_theme(&ctx, &t);
    }
    (ctx, t)
}

/// Runs a frame of `ui` at `size`, `dt` seconds after the last (whose time
/// `time` keeps), with `events`; its whole output.
pub(crate) fn run_frame(ctx: &egui::Context, size: egui::Vec2, time: &mut f64, dt: f64, events: Vec<egui::Event>, ui: impl FnMut(&mut Ui)) -> egui::FullOutput {
    *time += dt;
    let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)), time: Some(*time), events, ..Default::default() };
    let mut out = ctx.run_ui(input, ui);
    out.textures_delta.clear();
    out
}

/// The texts a frame painted, each with where it shows.
pub(crate) fn painted(out: &egui::FullOutput) -> Vec<(String, Rect)> {
    out.shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::Shape::Text(x) => Some((x.galley.text().to_string(), x.visual_bounding_rect())),
            _ => None,
        })
        .collect()
}

/// What a frame copied.
pub(crate) fn copied(out: &egui::FullOutput) -> Vec<String> {
    out.platform_output.commands.iter().filter_map(|c| if let egui::OutputCommand::CopyText(s) = c { Some(s.clone()) } else { None }).collect()
}

/// Where the painted text `s` shows.
pub(crate) fn rect_of(shapes: &[(String, Rect)], s: &str) -> Rect {
    shapes.iter().find(|(x, _)| x == s).map(|(_, r)| *r).unwrap_or_else(|| panic!("no {s:?} in {shapes:?}"))
}

/// `button` pressed (or let go) at `at`.
pub(crate) fn press(at: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton { pos: at, button, pressed, modifiers: Modifiers::NONE }
}

/// A click of `button` at `at`: moving there, pressing and letting go.
pub(crate) fn click(at: egui::Pos2, button: egui::PointerButton) -> Vec<egui::Event> {
    vec![egui::Event::PointerMoved(at), press(at, button, true), press(at, button, false)]
}

/// A key pressed with `modifiers`.
pub(crate) fn key(key: Key, modifiers: Modifiers) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}
