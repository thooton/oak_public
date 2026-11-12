//! The Settings page: theme, font (and a plugin font's download), the
//! interface's zoom (kept in settings.json, see `sync_zoom`), image zoom, touchpad scroll, prompt cache, account rotation, server threads,
//! the connections and the plugins; and the boxes that add a Codex
//! account, the OpenRouter or NodeClub key, or a local server.

use super::*;
use crate::widgets::{add_field, page_card, sign_in_heading};

use crate::theme::hex;
use crate::composer::{GEAR_ICON, side_box};

impl App {
    /// Settings, opened from the gear in a conversation's bottom left
    /// corner: one box in the conversation's own colors, under a heading
    /// with an X (or Escape) that goes back to where it was opened.
    pub(crate) fn settings_page(&mut self, ui: &mut Ui, t: &Theme) {
        let (card, head, closed) = page_card(ui, t, 780.0, None, "settings-close");
        let p = ui.painter();
        p.text(egui::pos2(head.left() + 14.0, head.center().y), Align2::LEFT_CENTER, GEAR_ICON, markdown::icon_font(13.0), t.ink3);
        p.text(egui::pos2(head.left() + 34.0, head.center().y), Align2::LEFT_CENTER, "Settings", FontId::proportional(13.0), t.ink);
        if closed {
            self.page = self.settings_from.take().filter(|p| *p != Page::Settings).unwrap_or(Page::Chats);
        }
        let body = Rect::from_min_max(egui::pos2(card.left() + 1.0, head.bottom() + 1.0), card.max - vec2(1.0, 1.0));
        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
            ScrollArea::vertical().id_salt("settings-scroll").auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin::symmetric(16, 16)).show(ui, |ui| {
                    egui::Grid::new("settings-grid").num_columns(2).spacing(vec2(28.0, 14.0)).min_col_width(70.0).show(ui, |ui| self.settings_rows(ui, t));
                });
            });
        });
    }

    /// The settings grid's rows, top to bottom.
    fn settings_rows(&mut self, ui: &mut Ui, t: &Theme) {
        self.theme_row(ui, t);
        self.font_row(ui, t);

        caption(ui, "Zoom", t.ink3);
        zoom_row(ui, self.settings.zoom);
        ui.end_row();

        let tip = "Cache writes: 5 minutes 1.25× input price; 1 hour 2×. Reads: 0.1×. Auto uses conversation timing.";
        if option_row(ui, t, "Prompt cache", &mut self.settings.cache, [("auto".to_string(), "Auto"), ("5m".to_string(), "5 minutes"), ("1h".to_string(), "1 hour")], Some(tip)) {
            api::set_cache_mode(&self.settings.cache);
            store::save_settings(&self.settings);
        }
        self.on_off_row(ui, t, "Server threads", |s| &mut s.threads, api::set_threads, "Experimental API. Sends only new messages; the server retains the conversation. Falls back to a full request if needed.");
        self.on_off_row(ui, t, "Keep cache warm", |s| &mut s.keep_alive, api::set_keep_alive, "Claude: rereads an idle conversation's 5-minute cache just before it expires (0.05× input each time), up to 30 minutes. With Auto, the cache is then always 5 minutes.");
        self.on_off_row(ui, t, "Cache diagnostics", |s| &mut s.cache_diagnostics, api::set_diagnostics, "Claude: each request names the previous one, and the API says why a request missed the cache (system, tools or messages changed). Logged in usage.jsonl.");
        self.on_off_row(ui, t, "Stable prefix", |s| &mut s.stable_prefix, api::set_stable_prefix, "A new date, AGENT model list, loaded kit or (Claude) effort reaches the model after the latest message, instead of a changed system prompt, tool list or effort that rewrites the whole cache; all are brought up to date when the cache is cold anyway. Claude and Codex use their own mid-conversation messages, which turn themselves off if refused; other providers get a text note.");

        if option_row(ui, t, "Title model", &mut self.settings.title_model, openrouter::TITLE_CHOICES.iter().map(|(id, name)| (id.to_string(), *name)), Some("Requires an OpenRouter key. Nemotron is free; Gemma costs a fraction of a cent per title. Each is the other's fallback.")) {
            openrouter::set_title_model(&self.settings.title_model);
            store::save_settings(&self.settings);
        }

        if option_row(ui, t, "OpenRouter routing", &mut self.settings.or_routing, openrouter::Routing::ALL.map(|r| (r.id().to_string(), r.label())), Some(
            "Which provider serves DeepSeek on OpenRouter. Balanced: cost times the square root of time. \
             Cheapest: cost per request, counting cache reads (most of an agent's input). Fastest: first token plus reply time. \
             OpenRouter: its own routing within OAK's price ceiling. A conversation stays with its provider while it is within 1.3× of the best, to keep its cache.",
        )) {
            openrouter::set_routing(&self.settings.or_routing);
            store::save_settings(&self.settings);
        }

        if option_row(ui, t, "Local summaries", &mut self.settings.local_summarizer, rolling::Summarizer::ALL.map(|s| (s.id().to_string(), s.label())), Some(
            "Local summaries stay on your server and pause the session. DeepSeek sends context to OpenRouter; \
             Haiku to Claude. Requires the corresponding account; falls back to local.",
        )) {
            rolling::set_summarizer(rolling::Summarizer::from_id(&self.settings.local_summarizer));
            store::save_settings(&self.settings);
        }

        // What plugins add to this page, each a row.
        let mut sections: Vec<_> = crate::plugins::parts().settings.iter().collect();
        sections.sort_by_key(|s| s.order());
        for section in sections {
            caption(ui, &section.title(), t.ink3);
            ui.vertical(|ui| {
                ui.set_max_width(520.0);
                section.show(ui, self);
            });
            ui.end_row();
        }

        // Each connection's state; they are added and removed on Home.
        caption(ui, "Connections", t.ink3);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (name, state, tip) in connections() {
                status_dot(ui, t, name, state).on_hover_text(tip);
            }
        });
        ui.end_row();

        self.on_off_row(ui, t, "Account rotation", |s| &mut s.rotation, api::set_rotation, "Switches to the account with the most remaining usage at a limit. Off: waits for the selected account to reset.");
        self.on_off_row(ui, t, "Notes reminder", |s| &mut s.notes_alert, crate::agent::set_notes_alert, "Model reminder to prune notes over about 1,700 tokens.");

        caption(ui, "Scratchpads", t.ink3);
        ui.horizontal(|ui| self.scratch_row(ui, t));
        ui.end_row();

        self.plugins_row(ui, t);

        if option_row(ui, t, "Image zoom", &mut self.settings.click_zoom, [(false, "Scroll"), (true, "Click")], Some("Scroll: wheel to zoom, right-drag to pan. Click: toggle zoom, drag to pan.")) {
            store::save_settings(&self.settings);
        }

        if cfg!(all(unix, not(target_os = "macos"))) {
            caption(ui, "Touchpad scroll", t.ink3);
            let r = ui.add(egui::Slider::new(&mut self.settings.touchpad_speed, 1.0..=8.0).step_by(0.5).suffix("×")).on_hover_text("Touchpad sensitivity; mouse wheel unchanged.");
            // Saved once a drag ends, not on every frame of it.
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                store::save_settings(&self.settings);
            }
            ui.end_row();
        }
    }

    /// An Off/On row for the switch `field`, which `apply` puts in force
    /// (and which is saved) when flipped.
    fn on_off_row(&mut self, ui: &mut Ui, t: &Theme, label: &str, field: fn(&mut Settings) -> &mut bool, apply: fn(bool), tip: &str) {
        if option_row(ui, t, label, field(&mut self.settings), OFF_ON, Some(tip)) {
            apply(*field(&mut self.settings));
            store::save_settings(&self.settings);
        }
    }

    /// The Theme row: Oak's, and those plugins add (the row shows once
    /// there is a choice).
    fn theme_row(&mut self, ui: &mut Ui, t: &Theme) {
        let themes = crate::theme::themes();
        // One whose plugin isn't loaded stays the setting; Oak's shows.
        let now = if themes.iter().any(|(id, _)| *id == self.settings.theme) { self.settings.theme.clone() } else { crate::theme::OAK.to_string() };
        if themes.len() < 2 {
            return;
        }
        caption(ui, "Theme", t.ink3);
        ui.horizontal(|ui| {
            for (id, name) in &themes {
                if ui.selectable_label(now == *id, name).clicked() && now != *id {
                    self.settings.theme = id.clone();
                    apply_theme(ui.ctx(), &crate::theme::theme(id));
                    store::save_settings(&self.settings);
                }
            }
        });
        ui.end_row();
    }

    /// The Font row: IBM Plex Sans, and those plugins add (one is fetched
    /// when picked the first time, with the download's progress or error).
    fn font_row(&mut self, ui: &mut Ui, t: &Theme) {
        let faces = markdown::faces();
        if faces.len() < 2 {
            return;
        }
        let job = self.font_job.lock().unwrap().clone();
        let now = if faces.iter().any(|(id, _)| *id == self.settings.font) { self.settings.font.clone() } else { markdown::PLEX.to_string() };
        caption(ui, "Font", t.ink3);
        ui.horizontal(|ui| {
            for (id, name) in &faces {
                if ui.selectable_label(now == *id, name).clicked() && now != *id {
                    self.settings.font = id.clone();
                    store::save_settings(&self.settings);
                    if !markdown::install_face(ui.ctx(), id) {
                        download_font(ui.ctx(), &self.font_job, id);
                    }
                }
            }
            if job.busy {
                ui.add(egui::Spinner::new().size(12.0).color(t.ink3));
                caption(ui, "downloading the font…", t.ink3);
            } else if let Some(e) = &job.error {
                caption(ui, e, t.error);
            }
        });
        ui.end_row();
    }

    /// The Plugins row: each library Oak looked at, and why one isn't
    /// running; where they are looked for.
    fn plugins_row(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "Plugins", t.ink3);
        ui.vertical(|ui| {
            ui.set_max_width(520.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            let found = crate::plugins::found();
            for f in &found {
                let file = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (name, about) = match &f.info {
                    Some(i) => (format!("{} {}", if i.name.is_empty() { &i.id } else { &i.name }, i.version), i.about.clone()),
                    None => (file.clone(), String::new()),
                };
                let row = ui.horizontal(|ui| {
                    ui.label(RichText::new(name).color(if f.trouble.is_some() { t.ink3 } else { t.ink2 }));
                    if f.trouble.is_none() && !about.is_empty() {
                        ui.add(egui::Label::new(RichText::new(&about).small().color(t.ink3)).truncate());
                    }
                });
                row.response.on_hover_text(f.path.display().to_string());
                if let Some(why) = &f.trouble {
                    ui.add(egui::Label::new(RichText::new(format!("Not running: {why}")).small().color(t.error)).wrap());
                }
            }
            let dirs = crate::plugins::dirs();
            let place = dirs.iter().find(|d| d.is_dir()).or(dirs.first()).map(|d| d.display().to_string()).unwrap_or_default();
            let none = if found.is_empty() { "None loaded. " } else { "" };
            let note = ui.add(egui::Label::new(RichText::new(format!("{none}Plugins are libraries in {place}, loaded when Oak starts.")).small().color(t.ink3)).wrap());
            note.on_hover_text(format!("Looked for in:\n{}\n\nA plugin runs with all of Oak's access to this computer: add only those you trust.", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join("\n")));
        });
        ui.end_row();
    }

    /// The Scratchpads row: their size, and how long idle before one is
    /// cleared.
    fn scratch_row(&mut self, ui: &mut Ui, t: &Theme) {
        let scan = crate::scratch::scan(Duration::from_secs(15), ui.ctx());
        let size = match scan.at {
            Some(_) => {
                let n = scan.pads.len();
                format!("{} in {n} folder{}", crate::scratch::fmt_bytes(scan.bytes()), crate::plural(n))
            }
            None => "measuring…".to_string(),
        };
        ui.label(RichText::new(size).color(t.ink2))
            .on_hover_text("Every session's .oak/tmp/tempN folder, in the working folders of all conversations.");
        ui.add_space(12.0);
        caption(ui, "Clear when idle for", t.ink3);
        let before = self.settings.scratch_clear;
        let shown = crate::scratch::AGES.iter().find(|a| a.0 == before).map_or("Never", |a| a.1);
        egui::ComboBox::from_id_salt("scratch-clear").selected_text(shown).show_ui(ui, |ui| {
            for (age, label) in crate::scratch::AGES {
                ui.selectable_value(&mut self.settings.scratch_clear, age, label);
            }
        })
        .response
        .on_hover_text("Empties a session's scratchpad once neither the session nor its files changed for this long. Sessions at work or with running tasks are skipped.");
        if before != self.settings.scratch_clear {
            crate::scratch::set_auto(self.settings.scratch_clear);
            store::save_settings(&self.settings);
        }
    }

    /// Signing in to Codex from Home: the box for the address the browser
    /// ends at (when nothing listened for it), the size of an account box.
    /// True once signed in.
    pub(crate) fn codex_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        let Some(login) = self.codex_login.take() else { return false };
        let (mut done, mut keep) = (false, true);
        match codex::login_result(&login) {
            Some(Ok(())) => done = true,
            Some(Err(e)) => self.codex_error = Some(e),
            // The browser comes back on another thread: look again soon.
            None => ui.ctx().request_repaint_after(Duration::from_millis(300)),
        }
        if !done {
            let (mut add, mut cancel) = (false, false);
            side_box(ui, t, w, |ui| {
                sign_in_heading(ui, t, "Add Codex account", &login.url);
                let edit = TextEdit::singleline(&mut self.codex_paste).id(Id::unique("add-codex-address")).hint_text("Callback URL");
                (add, cancel) = add_field(ui, t, edit, None, self.codex_error.as_ref(), |ui| {
                    if login.listening {
                        caption(ui, "Waiting for ChatGPT sign-in…", t.ink3);
                    } else {
                        ui.label(RichText::new("Paste the callback URL after sign-in.").small().color(t.ink3))
                            .on_hover_text("localhost:1455 is unavailable. Copy the callback URL even if the page fails to load.");
                    }
                });
            });
            if cancel {
                keep = false;
            } else if add && !self.codex_paste.trim().is_empty() {
                match codex::finish_login(&login, &self.codex_paste) {
                    Ok(()) => done = true,
                    Err(e) => self.codex_error = Some(e),
                }
            }
        }
        if done || !keep {
            self.codex_paste.clear();
            self.codex_error = None;
        } else {
            self.codex_login = Some(login);
        }
        done
    }

    /// Adding the user's own llama.cpp server from Home: its address (the
    /// launcher's `--port`). True once added.
    pub(crate) fn local_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        let (mut add, mut cancel) = (false, false);
        side_box(ui, t, w, |ui| {
            caption(ui, "Add local llama.cpp server", t.ink2);
            let edit = TextEdit::singleline(&mut self.local_url).id(Id::unique("add-local-url")).hint_text(crate::local::DEFAULT_URL);
            (add, cancel) = add_field(ui, t, edit, None, None, |_| {});
        });
        let done = add && !cancel;
        if done {
            crate::local::add(&self.local_url);
        }
        if cancel || done {
            self.adding_local = false;
            self.local_url.clear();
        }
        done
    }

    /// Adding the OpenRouter key from Home, in a box the size of an
    /// account box. True once added.
    pub(crate) fn openrouter_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        key_add_box(
            ui,
            t,
            w,
            ("Add OpenRouter key", "add-openrouter-key", "sk-or-v1-…"),
            || "openrouter.ai/keys".to_string(),
            &mut self.adding_openrouter,
            openrouter::set_key,
        )
    }

    /// Adding the NodeClub key from Home, as the OpenRouter one: checked
    /// with NodeClub, then kept on the vault when connected to
    /// one (for every OAK), else on this machine. True once added.
    pub(crate) fn nodeclub_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        key_add_box(
            ui,
            t,
            w,
            ("Add NodeClub key", "add-nodeclub-key", "sk-…"),
            || {
                let to = if api::signing_in_remotely() { format!("kept on {}", api::vault_name()) } else { "kept on this machine".to_string() };
                format!("api.nodeclub.ai, {to}")
            },
            &mut self.adding_nodeclub,
            crate::nodeclub::set_key,
        )
    }
}

/// A key being added from Home: its box is open, the key typed, and why
/// saving it failed.
#[derive(Default)]
pub(crate) struct KeyForm {
    pub(crate) open: bool,
    key: String,
    error: Option<String>,
}

/// The OpenRouter and NodeClub key boxes from Home: a caption, the key in a
/// password field, a line (its error, or `note`), and Add and Cancel.
/// The key goes to `set_key`; on success, the form closes. True once added.
fn key_add_box(ui: &mut Ui, t: &Theme, w: f32, (title, id, hint): (&str, &str, &str), note: impl Fn() -> String, form: &mut KeyForm, set_key: fn(&str) -> Result<(), String>) -> bool {
    let KeyForm { open: adding, key, error } = form;
    let (mut add, mut cancel) = (false, false);
    side_box(ui, t, w, |ui| {
        caption(ui, title, t.ink2);
        let edit = TextEdit::singleline(key).id(Id::unique(id)).hint_text(hint);
        (add, cancel) = add_field(ui, t, edit, Some(id), error.as_ref(), |ui| {
            caption(ui, &note(), t.ink3);
        });
    });
    let mut done = false;
    if cancel {
        *adding = false;
    } else if add && !key.trim().is_empty() {
        match set_key(key) {
            Ok(()) => {
                done = true;
                *adding = false;
            }
            Err(e) => *error = Some(e),
        }
    }
    if !*adding {
        ui.data_mut(|d| d.remove_temp::<bool>(crate::widgets::secret_key(id)));
        key.clear();
        *error = None;
    }
    done
}

/// Fetching a plugin's font.
#[derive(Clone, Default)]
pub(crate) struct FontJob {
    busy: bool,
    error: Option<String>,
    /// Fetched: install the chosen font again.
    pub(crate) reinstall: bool,
}

/// Fetches the plugin font `face` in the background; the next frame
/// installs it if it is still the chosen font.
pub(crate) fn download_font(ctx: &egui::Context, job: &Arc<Mutex<FontJob>>, face: &str) {
    let Some(font) = markdown::plugin_face(face) else { return };
    let mut j = job.lock().unwrap();
    if j.busy {
        return;
    }
    j.busy = true;
    j.error = None;
    drop(j);
    let (job, ctx) = (job.clone(), ctx.clone());
    std::thread::spawn(move || {
        let r = font.fetch();
        let mut j = job.lock().unwrap();
        j.busy = false;
        j.error = r.err();
        j.reinstall = j.error.is_none();
        ctx.request_repaint();
    });
}

/// How a connection is: working, failing, or not set up.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Conn {
    Ok,
    Failing,
    Off,
}

/// The connections Settings lists: Claude, Codex, OpenRouter, NodeClub,
/// Freebuff and the local llama.cpp server, how each is, and what its dot's
/// hover says.
fn connections() -> [(&'static str, Conn, String); 6] {
    let kept = |remote: bool| if remote { " (shared)" } else if api::vault_on() { " (on this machine)" } else { "" };
    let accounts = api::accounts();
    let claude = if accounts.is_empty() {
        (Conn::Off, "No Claude account: add one on Home.".to_string())
    } else if let Some(e) = accounts.iter().all(|a| a.error.is_some() && a.limits.is_empty()).then(|| accounts[0].error.clone().unwrap_or_default()) {
        (Conn::Failing, e)
    } else {
        let n = accounts.len();
        (Conn::Ok, format!("{n} account{}", crate::plural(n)))
    };
    let codex = match codex::account() {
        None => (Conn::Off, "No Codex account: add one on Home.".to_string()),
        Some(a) => match a.error.filter(|_| a.limits.is_empty()) {
            Some(e) => (Conn::Failing, e),
            None => {
                let who: Vec<&str> = [a.email.as_str(), a.plan.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
                (Conn::Ok, format!("{}{}", if who.is_empty() { "Signed in".to_string() } else { who.join(", ") }, kept(codex::remote())))
            }
        },
    };
    let router = match openrouter::account() {
        None => (Conn::Off, "No OpenRouter key: add one on Home.".to_string()),
        Some(a) => match a.error {
            Some(e) => (Conn::Failing, e),
            None => (Conn::Ok, format!("{}{}", if a.label.is_empty() { "API key" } else { &a.label }, kept(a.remote))),
        },
    };
    let nodeclub = match crate::nodeclub::account() {
        None => (Conn::Off, "No NodeClub key: add one on Home.".to_string()),
        Some(a) => match a.error {
            Some(e) => (Conn::Failing, e),
            None => (Conn::Ok, format!("{}{}", if a.label.ends_with(" keys") { a.label.clone() } else { format!("key {}", a.label) }, kept(a.remote))),
        },
    };
    let freebuff = match freebuff::account() {
        None => (Conn::Off, "No Freebuff account: add one on Home.".to_string()),
        Some((email, _)) => match freebuff::error() {
            Some(e) => (Conn::Failing, e),
            None => (Conn::Ok, if email.is_empty() { "Signed in".to_string() } else { email }),
        },
    };
    // Its server is the user's to start: what is known here is that one was
    // added, and where it is.
    let llama = match local::server() {
        None => (Conn::Off, "No local llama.cpp server: add one on Home.".to_string()),
        Some(s) => (Conn::Ok, format!("{}, {}", if s.alias.is_empty() { "llama.cpp" } else { &s.alias }, s.url)),
    };
    [("Claude", claude.0, claude.1), ("Codex", codex.0, codex.1), ("OpenRouter", router.0, router.1),
        ("NodeClub", nodeclub.0, nodeclub.1),
        ("Freebuff", freebuff.0, freebuff.1), ("Local", llama.0, llama.1)]
}

/// The zoom factors Oak takes, as egui's own Ctrl+Plus and Ctrl+Minus.
const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.2..=5.0;

/// A saved zoom factor made safe to apply: within `ZOOM_RANGE`, 100% when
/// it is no number.
pub(crate) fn saved_zoom(z: f32) -> f32 {
    if z.is_finite() { z.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end()) } else { 1.0 }
}

impl App {
    /// Keeps the interface's zoom in settings.json: egui changes it itself
    /// (Ctrl+Plus, Ctrl+=, Ctrl+Minus, Ctrl+0, at the end of a frame) and the
    /// Settings row asks for it, so each frame saves what is new since the
    /// last one.
    pub(crate) fn sync_zoom(&mut self, ctx: &egui::Context) {
        let z = ctx.zoom_factor();
        if z != self.settings.zoom {
            self.settings.zoom = z;
            store::save_settings(&self.settings);
        }
    }
}

/// The Zoom row: smaller, the zoom, larger and back to 100%, with the
/// keyboard shortcuts on hover.
fn zoom_row(ui: &mut Ui, zoom: f32) {
    let ctx = ui.ctx().clone();
    let shortcut = |s: &egui::KeyboardShortcut| ctx.format_shortcut(s);
    use egui::gui_zoom::kb_shortcuts as kb;
    ui.horizontal(|ui| {
        let out = ui.add_enabled(zoom > *ZOOM_RANGE.start() + 0.01, egui::Button::new("−").min_size(vec2(26.0, 0.0)));
        if out.on_hover_text(format!("Zoom out ({})", shortcut(&kb::ZOOM_OUT))).clicked() {
            egui::gui_zoom::zoom_out(&ctx);
        }
        let percent = format!("{:.0}%", zoom * 100.0);
        ui.add_sized(vec2(44.0, ui.spacing().interact_size.y), egui::Label::new(percent));
        let into = ui.add_enabled(zoom < *ZOOM_RANGE.end() - 0.01, egui::Button::new("+").min_size(vec2(26.0, 0.0)));
        if into.on_hover_text(format!("Zoom in ({})", shortcut(&kb::ZOOM_IN))).clicked() {
            egui::gui_zoom::zoom_in(&ctx);
        }
        let reset = ui.add_enabled(zoom != 1.0, egui::Button::new("Reset"));
        if reset.on_hover_text(format!("100% ({})", shortcut(&kb::ZOOM_RESET))).clicked() {
            ctx.set_zoom_factor(1.0);
        }
    });
}

/// One setting's caption and its options in a row (`tip` on the row), in
/// the settings grid. True when another was picked.
fn option_row<T: PartialEq + Clone>(ui: &mut Ui, t: &Theme, label: &str, value: &mut T, choices: impl IntoIterator<Item = (T, &'static str)>, tip: Option<&str>) -> bool {
    caption(ui, label, t.ink3);
    let before = value.clone();
    let r = ui.horizontal(|ui| {
        for (v, label) in choices {
            ui.selectable_value(value, v, label);
        }
    });
    if let Some(tip) = tip {
        r.response.on_hover_text(tip);
    }
    ui.end_row();
    before != *value
}

/// A switch's choices.
const OFF_ON: [(bool, &str); 2] = [(false, "Off"), (true, "On")];

/// A dot (green: working, red: failing, grey: not set up) and a name.
fn status_dot(ui: &mut Ui, t: &Theme, name: &str, state: Conn) -> egui::Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        let color = match state {
            Conn::Ok => hex(0x5fb36b),
            Conn::Failing => t.error,
            Conn::Off => t.ink4,
        };
        ui.painter().circle_filled(r.center(), 4.0, color);
        ui.label(RichText::new(name).color(if state == Conn::Off { t.ink3 } else { t.ink }));
    })
    .response
}

#[cfg(test)]
mod tests {
    /// A saved zoom is applied as it was, within egui's own limits, and an
    /// unusable one is 100%.
    #[test]
    fn saved_zoom_is_kept_within_range() {
        assert_eq!(super::saved_zoom(1.3), 1.3);
        assert_eq!(super::saved_zoom(0.0), 0.2);
        assert_eq!(super::saved_zoom(40.0), 5.0);
        assert_eq!(super::saved_zoom(f32::NAN), 1.0);
    }
}
