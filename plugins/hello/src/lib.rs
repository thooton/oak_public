//! The example plugin: a tab on Home, a theme, a row on Settings and a
//! system prompt. Build it with `cargo build -p oak-hello` and run Oak with
//! `OAK_PLUGINS=target/debug` (or copy the library into the `plugins`
//! folder of Oak's folder); `docs/PLUGINS.md` explains the rest.
//!
//! `oak_sdk::egui` has egui's names, but nothing of egui is in this
//! library: each call goes to Oak, which draws.

use oak_sdk::egui::{RichText, Ui};
use oak_sdk::theme::Palette;
use oak_sdk::widgets::{caption, column};
use oak_sdk::{App, HomeTab, Info, Plugin, Registrar, Role, SettingsSection, ThemeDef, host};
use std::sync::atomic::{AtomicU32, Ordering};

struct Hello;

impl Plugin for Hello {
    fn info(&self) -> Info {
        Info::new("hello", "Hello", env!("CARGO_PKG_VERSION"), "The example plugin: a Home tab, a theme and a system prompt.")
    }

    fn register(&self, r: &mut Registrar) {
        r.home_tab(HelloTab);
        r.theme(Moss);
        r.settings(HelloSettings);
        r.role(Role {
            id: "haiku".into(),
            name: "Haiku".into(),
            // `{{tools}}` is where Oak's tool instructions go.
            text: "You are Oak, a coding agent that answers in haiku whenever no code is asked for.\n\n{{tools}}".into(),
            offered: true,
            icon: String::new(),
        });
    }
}

oak_sdk::export_plugin!(Hello);

/// How often the button was clicked: a plugin's state is its own (statics,
/// or fields behind locks: parts are shared between threads).
static CLICKS: AtomicU32 = AtomicU32::new(0);

/// A tab in Home's left cell.
struct HelloTab;

impl HomeTab for HelloTab {
    fn id(&self) -> String {
        "hello".into()
    }

    fn label(&self) -> String {
        "Hello".into()
    }

    fn tip(&self) -> String {
        "The example plugin's tab".into()
    }

    fn show(&self, ui: &mut Ui, app: &mut dyn App) {
        // The colors come from the theme in use, whichever it is.
        let t = app.theme();
        column(ui, 420.0, |ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("Hello from a plugin").size(18.0).color(t.accent));
            ui.add_space(6.0);
            let n = app.conversations().len();
            ui.label(RichText::new(format!("Oak {} has {n} conversation{} on this machine.", host().version(), oak_sdk::plural(n))).color(t.ink2));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Count").clicked() {
                    CLICKS.fetch_add(1, Ordering::Relaxed);
                }
                caption(ui, &format!("clicked {} times", CLICKS.load(Ordering::Relaxed)), t.ink3);
            });
            ui.add_space(12.0);
            if ui.button("New conversation").clicked() {
                // What a plugin does to the window goes through `app`.
                app.go_new("local");
            }
        });
    }
}

/// A theme: colors only.
struct Moss;

impl ThemeDef for Moss {
    fn id(&self) -> String {
        "moss".into()
    }

    fn name(&self) -> String {
        "Moss".into()
    }

    fn palette(&self) -> Palette {
        Palette {
            bg: 0x0d1410,
            panel: 0x141d17,
            panel2: 0x1c2820,
            ink: 0xeef3ea,
            ink2: 0xb9c6b6,
            ink3: 0x7d8f7c,
            ink4: 0x516052,
            accent: 0x9ccc65,
            error: 0xe5735f,
            line_alpha: 0x26,
        }
    }
}

/// A row on the Settings page.
struct HelloSettings;

impl SettingsSection for HelloSettings {
    fn title(&self) -> String {
        "Hello".into()
    }

    fn show(&self, ui: &mut Ui, app: &mut dyn App) {
        let t = app.theme();
        ui.horizontal(|ui| {
            if ui.button("Open its tab").clicked() {
                app.go_home("hello");
            }
            caption(ui, "A row the example plugin adds.", t.ink3);
        });
    }
}
