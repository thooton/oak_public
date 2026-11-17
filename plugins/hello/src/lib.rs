//! The example plugin: a tab on Home, a theme, a row on Settings and a
//! system prompt. Build it with `cargo build -p oak-hello` and run Oak with
//! `OAK_PLUGINS=target/debug` (or copy the library into the `plugins`
//! folder of Oak's folder); `docs/PLUGINS.md` explains the rest.
//!
//! `oak_sdk::view` has Oak's components (pages, sections, buttons, fields,
//! switches, toasts) and what keeps a plugin's state; `oak_sdk::egui` has
//! egui's names for the rest. Nothing of egui is in this library: each
//! call goes to Oak, which draws.

use oak_sdk::egui::Ui;
use oak_sdk::theme::Palette;
use oak_sdk::view::{Action, Field, Job, Notice, Page, Section, State, Stored, Toggle, Tone, setting, toast};
use oak_sdk::{App, HomeTab, Info, Plugin, Registrar, Role, SettingsSection, ThemeDef, host};
use std::path::Path;

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

/// What the tab keeps while Oak runs: a plugin's state is its own
/// (statics, or fields behind locks: parts are shared between threads).
#[derive(Default)]
struct Tab {
    clicks: u32,
    /// The name being typed.
    name: String,
    /// The last measure of Oak's folder.
    measured: Option<Result<String, String>>,
}

static TAB: State<Tab> = State::new();

/// What it keeps from one run of Oak to the next (in Oak's folder).
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct Prefs {
    name: String,
    hide_counter: bool,
}

static PREFS: Stored<Prefs> = Stored::new("prefs");

/// Work on a thread of its own: how much is in Oak's folder.
static MEASURE: Job<String> = Job::new();

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
        let n = app.conversations().len();
        let about = format!("Oak {} has {n} conversation{} on this machine.", host().version(), oak_sdk::plural(n));
        let mut new_conversation = false;
        let mut prefs = PREFS.get();
        TAB.with(|s| {
            if let Some(done) = MEASURE.take() {
                s.measured = Some(done);
            }
            Page::new("Hello from a plugin")
                .subtitle(&about)
                .width(560.0)
                .actions(|ui| {
                    // What a plugin does to the window goes through `app`,
                    // after the page is drawn.
                    new_conversation = Action::new("New conversation").show(ui).clicked();
                })
                .show(ui, |ui| {
                    if !prefs.hide_counter {
                        Section::new("Counter").card().show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if Action::primary("Count").show(ui).clicked() {
                                    s.clicks += 1;
                                    if s.clicks % 10 == 0 {
                                        toast(Tone::Good, &format!("{} clicks", s.clicks));
                                    }
                                }
                                ui.label(format!("clicked {} times", s.clicks));
                            });
                        });
                    }
                    Section::new("You").card().show(ui, |ui| {
                        if s.name.is_empty() {
                            s.name = prefs.name.clone();
                        }
                        let r = Field::new(&mut s.name).label("Your name").hint("What Oak calls you here").button("Keep").show(ui);
                        if r.confirmed() {
                            prefs.name = s.name.trim().to_string();
                            toast(Tone::Good, &format!("Kept {}", prefs.name));
                        }
                        ui.add_space(6.0);
                        let mut show = !prefs.hide_counter;
                        if Toggle::new(&mut show, "Show the counter").help("Kept for the next time Oak starts.").show(ui).changed() {
                            prefs.hide_counter = !show;
                        }
                    });
                    Section::new("Oak's folder").card().show(ui, |ui| {
                        let busy = MEASURE.busy();
                        match &s.measured {
                            _ if busy => Notice::new(Tone::Plain, "Measuring...").busy().show(ui),
                            Some(Ok(size)) => Notice::new(Tone::Good, size).show(ui),
                            Some(Err(e)) => Notice::new(Tone::Bad, e).show(ui),
                            None => Notice::new(Tone::Plain, "Not measured yet.").show(ui),
                        };
                        ui.add_space(6.0);
                        if Action::new("Measure").busy(busy).show(ui).clicked() {
                            let dir = host().dir();
                            MEASURE.start(move || measure(&dir));
                        }
                    });
                });
        });
        // (Kept only when it changed.)
        if prefs != PREFS.get() {
            PREFS.set(prefs);
        }
        if new_conversation {
            app.go_new("local");
        }
    }
}

/// How much is in `dir`, and in how many files.
fn measure(dir: &Path) -> Result<String, String> {
    fn walk(dir: &Path, bytes: &mut u64, files: &mut u64) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let kind = e.file_type()?;
            if kind.is_dir() {
                walk(&e.path(), bytes, files)?;
            } else if kind.is_file() {
                *bytes += e.metadata()?.len();
                *files += 1;
            }
        }
        Ok(())
    }
    let (mut bytes, mut files) = (0, 0);
    walk(dir, &mut bytes, &mut files).map_err(|e| format!("Couldn't read {}: {e}", dir.display()))?;
    Ok(format!("{} in {files} file{}", oak_sdk::ui::size_text(bytes), oak_sdk::plural(files)))
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
        let mut open = false;
        setting(ui, "Example plugin", "A row the example plugin adds.", |ui| {
            open = Action::new("Open its tab").show(ui).clicked();
        });
        if open {
            app.go_home("hello");
        }
    }
}

