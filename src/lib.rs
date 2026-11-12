//! Oak's app: its state (`App`), its screens and the frame loop, the crash
//! log, and `main` (the binary, `main.rs`, only calls it). Each part of the
//! window has a file of its own.

/// A setting kept for the session: `$get()` reads it, `$set(on)` changes it.
macro_rules! switch {
    ($flag:ident, $get:ident, $set:ident, $default:expr) => {
        static $flag: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new($default);

        pub fn $set(on: bool) {
            $flag.store(on, std::sync::atomic::Ordering::Relaxed);
        }

        pub fn $get() -> bool {
            $flag.load(std::sync::atomic::Ordering::Relaxed)
        }
    };
}

mod agent;
mod api;
mod cache_ttl;
mod bench;
mod browser;
mod cdp;
mod changes;
mod cloud;
mod codex;
mod duck;
mod freebuff;
mod freebuff_tools;
mod gateway;
mod images;
mod instructions;
mod local;
mod measure;
mod nodeclub;
mod oak;
mod openrouter;
mod prefix;
mod progress;
mod provider;
mod providers;
mod providers_view;
mod rolling;
mod rotation;
mod scratch;
mod guard;
mod sleeps;
mod store;
mod tasks;
mod time;
mod token_rate;
mod tools;
mod unslop;
mod update;
mod usage;
mod web;
mod wire_anthropic;
#[cfg(windows)]
mod winctl;

// The window's parts, drawn from the state above.
mod browser_view;
mod autoscroll;
mod chat_view;
mod cloud_view;
mod composer;
mod conversations;
mod copy_menu;
mod find;
mod geometry;
mod gpu;
mod duck_view;
mod highlight;
mod home;
mod markdown;
mod new_screen;
mod select;
mod pausing;
mod plugin_app;
pub mod plugins;
mod rename;
mod resets_view;
mod session_box;
mod side_chat;
mod settings;
mod tabs;
mod text_menu;
mod theme;
mod transcript;
mod usage_view;
mod vlist;
mod widgets;
mod window;

use crate::tools::Head;
pub(crate) use oak_sdk::{hash_of, plural};
use agent::{BEAT_TAG, Command, Live, NOTE_TAG, Session};
use eframe::egui::{
    self, Align, Align2, Button, Color32, CornerRadius, FontId, Id, Key, KeyboardShortcut, Layout, Margin,
    Modifiers, Rangef, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder, vec2,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use chat_view::chat_view;
use cloud_view::{cloud_chat_view, cloud_list_view, cloud_new_info, provider_icon};
use composer::{SizeInputs, chat_column};
use conversations::{Editing, QueueAction};
use home::{AccountState, AddAccount, paint_cover, picture};
use settings::{FontJob, download_font};
use store::{Conversation, Mark, MarkKind, Settings};
use tabs::TABS_H;
use theme::{Theme, apply_theme, setup_style};
use time::{ago, clock, full_time, local_offset_ms, mdy, time_ago, until};
use transcript::{Item, JUMP_REQUEST, REVERT_REQUEST, REWIND_REQUEST, ask_request_view, ask_view, attachment_view, file_block, items_at, queue_view, size_text, tail_sig, tool_summary, transcript_status, transcript_unit, unit_sig, units, user_messages};
use widgets::{caption, column};
#[cfg(target_os = "linux")]
use window::install_desktop_entry;
use window::{ICON_PNG, TITLE_H, title_bar, window_edges};

/// Points scrolled per mouse-wheel notch, as Chrome and Edge scroll on
/// Windows: 100/3 points for each line the system's wheel setting scrolls
/// (100 at the default of 3 lines); a setting of "a screen at a time"
/// scrolls about a screen.
fn wheel_notch() -> f32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWHEELSCROLLLINES, SystemParametersInfoW};
        let mut lines: u32 = 3;
        // SAFETY: the call writes one u32.
        let ok = unsafe { SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, (&mut lines as *mut u32).cast(), 0) };
        if ok != 0 {
            // WHEEL_PAGESCROLL: a screen at a time.
            return if lines == u32::MAX { 600.0 } else { lines.clamp(1, 100) as f32 * 100.0 / 3.0 };
        }
    }
    100.0
}

/// Scales the touchpad's scrolling by `speed`. On Linux a touchpad's
/// scroll arrives in pixels (`Point`s), which Wayland passes on 1:1 with
/// the fingers' travel: far slower than a wheel's notches (`Line`s, see
/// `wheel_notch`), which stay as they are.
fn speed_up_touchpad(raw: &mut egui::RawInput, speed: f32) {
    let speed = if speed.is_finite() { speed.clamp(0.25, 20.0) } else { 1.0 };
    for e in &mut raw.events {
        if let egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } = e {
            *delta *= speed;
        }
    }
}

/// The New conversation screen, or the open conversation (by id).
#[derive(Default)]
enum Screen {
    #[default]
    New,
    Chat(String),
}

/// Home (the forest, with the chats and accounts box), Settings, or an
/// open conversation tab.
#[derive(Clone, Copy, Default, PartialEq)]
enum Page {
    #[default]
    Home,
    Settings,
    Chats,
    /// Charts of the usage log, opened from Home's preview.
    Usage,
    /// A private duck.ai chat, opened from Home (`duck_view.rs`).
    Duck,
}

/// A quick hasher (FxHash's step) for what is hashed every frame: the
/// transcript's units' signatures and its maps of tool calls, where
/// SipHash, built to resist chosen keys, took most of the time. Each step
/// is a bijection of the state, so a change in any one field always
/// changes the hash.
#[derive(Default)]
pub(crate) struct QuickHasher(u64);

/// The fields `keys` of the JSON object `v` (None where missing), read in
/// one pass over it. What the transcript reads of each block every frame:
/// a lookup by key walks the object's sorted keys comparing their bytes,
/// so several lookups cost several walks, where one pass mostly compares
/// lengths.
pub(crate) fn fields<'a, const N: usize>(v: &'a serde_json::Value, keys: [&str; N]) -> [Option<&'a serde_json::Value>; N] {
    let mut out = [None; N];
    if let serde_json::Value::Object(o) = v {
        for (k, x) in o {
            if let Some(i) = keys.iter().position(|q| *q == k.as_str()) {
                out[i] = Some(x);
            }
        }
    }
    out
}

/// A map or set hashed with `QuickHasher`.
pub(crate) type QuickState = std::hash::BuildHasherDefault<QuickHasher>;

impl QuickHasher {
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl std::hash::Hasher for QuickHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last));
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.add(n as u64);
    }
    fn write_u32(&mut self, n: u32) {
        self.add(n as u64);
    }
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type Models = Arc<Mutex<Option<Result<Vec<api::Model>, String>>>>;

/// The window's state; its default is every field at its start: nothing
/// loaded, nothing typed, Home shown.
#[derive(Default)]
struct App {
    settings: Settings,
    signed_in: bool,
    /// Why removing an account or moving it to the vault failed.
    account_note: Option<String>,
    /// A removal or move of accounts, running in the background.
    account_job: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// The providers whose accounts show under their box on Home.
    providers_open: Vec<api::Provider>,
    /// The OpenRouter and NodeClub keys being added from Home.
    adding_openrouter: settings::KeyForm,
    adding_nodeclub: settings::KeyForm,
    adding_local: bool,
    local_url: String,
    /// A Codex sign-in under way from Home; the address pasted for it; why
    /// the last one failed.
    codex_login: Option<codex::Login>,
    codex_paste: String,
    codex_error: Option<String>,
    sessions: HashMap<String, Session>,
    /// The retained side conversation open in the right-hand pane.
    side_chat: Option<String>,
    side_deleting: bool,
    side_focus: bool,
    side_browser: bool,
    side_active: bool,
    side_hint: Option<String>,
    side_editing: Option<Editing>,
    models: Models,
    screen: Screen,
    page: Page,
    /// The page Settings was opened from: its X goes back there.
    settings_from: Option<Page>,
    /// The message being written, and its attachments: the open tab's
    /// draft (each conversation, and the new one, keeps its own).
    input: String,
    hint: Option<String>,
    editing: Option<Editing>,
    size_inputs: SizeInputs,
    /// Images attached to the message being written, as content blocks.
    attachments: Vec<Value>,
    account: Arc<Mutex<AccountState>>,
    /// Signing in another account from Home's accounts column.
    adding: Option<AddAccount>,
    /// A plugin's font being downloaded, or why that failed.
    font_job: Arc<Mutex<FontJob>>,
    /// Pictures built into OAK, loaded the first time they are shown.
    pictures: HashMap<&'static str, egui::TextureHandle>,
    /// The other tabs' drafts: text and attachments, by conversation id
    /// ("" for the new conversation).
    drafts: HashMap<String, (String, Vec<Value>)>,
    /// Whose draft `input` and `attachments` are.
    draft_key: String,
    /// The text last sent from each tab's message box, by the same keys as
    /// `drafts`: Up in the empty box brings it back (`composer::recall`).
    last_sent: HashMap<String, String>,
    /// The goal being edited in the session box: (conversation, text,
    /// focus it next frame).
    goal_edit: Option<(String, String, bool)>,
    /// A model switch waiting for the user to confirm it: (conversation,
    /// model id).
    model_switch: Option<(String, api::Provider, String)>,
    /// A conversation on Home waiting for the user to confirm deleting it.
    deleting_chat: Option<String>,
    /// The usage-limit reset whose dialog is open (`resets_view`).
    reset_dialog: Option<resets_view::ResetDialog>,
    /// The conversation being renamed (`rename.rs`).
    renaming: Option<rename::Renaming>,
    /// The transcript's scroll bar and middle-click scrolling: each
    /// conversation's transcript list (its heights and scroll position).
    lists: HashMap<String, vlist::List>,
    /// The Usage page's filters and what it last worked out.
    usage: usage_view::View,
    /// What Home's left cell shows: this machine's conversations
    /// (`home::LOCAL_TAB`, also when empty), the cloud sessions
    /// (`home::CLOUD_TAB`), or a plugin's tab, by its id.
    home_list: String,
    /// What Home's conversation search narrows the list to (not kept).
    chat_filter: String,
    /// The New screen starts a cloud session.
    new_cloud: bool,
    new_provider: cloud::Provider,
    /// The New screen starts what a plugin adds (`oak_sdk::NewKind`): its id.
    new_kind: Option<String>,
    /// A cloud session being started or moved: whose draft it was, and the
    /// message and attachments, put back there if it fails.
    cloud_pending: Option<(String, String, Vec<Value>)>,
    /// A tab being dragged to a new place: its conversation, and where the
    /// pointer took hold of it (from the tab's left edge).
    tab_drag: Option<(String, f32)>,
    /// How far the tab strip is scrolled, when its tabs don't all fit.
    tab_scroll: f32,
    /// The tab last scrolled into view: the strip shows a tab when it
    /// becomes the one shown (or the strip changes size), then leaves the
    /// scrolling to the user.
    tab_revealed: Option<String>,
    /// The strip's room and tab width when it was revealed: a resize, or a
    /// tab opened or closed, reveals it again.
    tab_revealed_at: (f32, f32),
    /// Tabs closed this run, oldest first, each with where it was: Ctrl+Shift+T
    /// reopens the newest. Never stored.
    closed_tabs: Vec<(usize, String)>,
    /// Pauses that took a session's subagents along, by session
    /// (`pausing.rs`).
    group_pauses: HashMap<String, pausing::GroupPause>,
    /// Closing the window with conversations at work (`pausing.rs`).
    closing: Option<pausing::Closing>,
    /// Conversations paused when OAK last closed, offered for resuming.
    resume_offer: Vec<String>,
    /// The Duck.ai page's message being written and its attachments, and
    /// why the last couldn't be sent (`duck_view.rs`). Never stored.
    duck_input: String,
    duck_files: Vec<Value>,
    duck_hint: Option<String>,
    /// The transcripts plugins have Oak draw (`oak_sdk::App::transcript`),
    /// by tab: the version each was made from, and it as a conversation.
    plugin_chats: HashMap<String, (u64, Conversation, Live)>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> App {
        gpu::watch(cc);
        let settings = store::load_settings();
        rolling::set_config(rolling::Config { trigger: settings.roll_trigger, keep: settings.roll_keep });
        rolling::set_summarizer(rolling::Summarizer::from_id(&settings.local_summarizer));
        api::set_cache_mode(&settings.cache);
        api::set_threads(settings.threads);
        api::set_keep_alive(settings.keep_alive);
        api::set_diagnostics(settings.cache_diagnostics);
        api::set_stable_prefix(settings.stable_prefix);
        api::set_rotation(settings.rotation);
        agent::set_notes_alert(settings.notes_alert);
        openrouter::set_title_model(&settings.title_model);
        openrouter::set_routing(&settings.or_routing);
        unslop::set_all(&settings);
        scratch::set_auto(settings.scratch_clear);
        let font_job = Arc::new(Mutex::new(FontJob::default()));
        if !markdown::install_face(&cc.egui_ctx, &settings.font) {
            // Chosen, but its files aren't here (yet, or any more): fetch them.
            download_font(&cc.egui_ctx, &font_job, &settings.font);
        }
        setup_style(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(settings::saved_zoom(settings.zoom));
        copy_menu::install(&cc.egui_ctx);
        autoscroll::install(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, &theme::theme(&settings.theme));
        // Before the sessions start: one resuming at once (after a usage
        // limit) sends its request through these accounts.
        let signed_in = api::load_auth();
        if !cfg!(test) {
            gateway::start_in_app();
        }
        // Calls left without a result and unfinished rewrites are looked for
        // in every conversation as it starts (`Session::start`): in the
        // loading threads first, as that reads them all.
        let conversations = store::load_conversations_then(&|c| {
            agent::look_for_lost_calls(c);
            unslop::look(c);
        });
        let sessions = conversations
            .into_iter()
            .map(|c| (c.id.clone(), Session::start(c, cc.egui_ctx.clone())))
            .collect();
        // Their tasks left running when OAK closed are theirs again.
        if !cfg!(test) {
            tasks::release_holder();
        }
        let mut app = App {
            settings,
            signed_in,
            sessions,
            font_job,
            ..App::default()
        };
        // (Not under test: the tests share the real settings.)
        if !cfg!(test) {
            app.take_resume_offer();
        }
        update::start(&cc.egui_ctx);
        if app.signed_in {
            app.fetch_models(&cc.egui_ctx);
        }
        // The plugins' turn: the window is up.
        plugins::started(&cc.egui_ctx);
        app
    }

    fn fetch_models(&self, ctx: &egui::Context) {
        *self.models.lock().unwrap() = None;
        let (models, ctx) = (self.models.clone(), ctx.clone());
        std::thread::spawn(move || {
            *models.lock().unwrap() = Some(api::latest_models());
            ctx.request_repaint();
        });
    }

    /// An account was added or removed, or a vault's came or went: the
    /// models and the accounts' usage are fetched again.
    pub(crate) fn accounts_changed(&mut self, ctx: &egui::Context) {
        self.signed_in = true;
        self.fetch_models(ctx);
        self.account.lock().unwrap().fetched = None;
    }

    /// Shows conversation `id`, in its tab (added if it has none).
    fn open(&mut self, id: String) {
        if !self.settings.tabs.contains(&id) {
            self.settings.tabs.push(id.clone());
            store::save_settings(&self.settings);
        }
        self.screen = Screen::Chat(id);
        self.page = Page::Chats;
    }

    /// Fetches every account's usage now and then: every two minutes,
    /// after a rotation, or sooner when `fetched` is cleared.
    fn refresh_account(&self, ctx: &egui::Context) {
        let services = (openrouter::on(), codex::on(), freebuff::on(), local::on(), nodeclub::on());
        let mut a = self.account.lock().unwrap();
        // The first fetch can come before the vault's accounts
        // have: an account arriving (or added) fetches the list again.
        let claude = !api::accounts().is_empty();
        if a.services.is_some_and(|s| s != services) || (claude && a.claude == Some(false)) {
            self.fetch_models(ctx);
        }
        a.services = Some(services);
        a.claude = Some(claude);
        let rotations = api::rotations();
        let due = a.fetched.is_none_or(|t| t.elapsed() > Duration::from_secs(120)) || a.rotations != rotations;
        if !due || a.loading {
            return;
        }
        a.loading = true;
        a.rotations = rotations;
        drop(a);
        let (account, ctx) = (self.account.clone(), ctx.clone());
        std::thread::spawn(move || {
            api::refresh_accounts();
            let mut a = account.lock().unwrap();
            a.fetched = Some(Instant::now());
            a.loading = false;
            ctx.request_repaint();
        });
    }

    fn model_name(&self, id: &str) -> String {
        let served = [
            openrouter::model(id).map(|m| m.name),
            codex::model(id).map(|m| m.name),
            freebuff::model(id).map(|m| m.name),
            nodeclub::model(id).map(|m| m.name),
        ];
        if local::serves(id) {
            return local::model_name(id);
        }
        if let Some(name) = served.into_iter().flatten().next() {
            return name.to_string();
        }
        match &*self.models.lock().unwrap() {
            Some(Ok(list)) => list.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or(id.to_string()),
            _ => id.to_string(),
        }
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if cfg!(all(unix, not(target_os = "macos"))) {
            speed_up_touchpad(raw, self.settings.touchpad_speed);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.sync_zoom(ui.ctx());
        let t = theme::theme(&self.settings.theme);
        let plain = egui::Frame::new().fill(t.bg);
        // A clicked image, over everything but the title bar.
        let style = images::ViewerStyle { ink: t.ink, ink3: t.ink3, line: t.line, panel: t.panel };
        let mode = if self.settings.click_zoom { images::ZoomMode::Click } else { images::ZoomMode::Scroll };
        images::viewer(ui.ctx(), TITLE_H, &style, mode);
        images::forget_unseen(ui.ctx());
        // Ctrl+A in a message's text selects the whole message (before the
        // labels see the keys).
        select::begin(ui);
        if std::mem::take(&mut self.font_job.lock().unwrap().reinstall) {
            markdown::install_face(ui.ctx(), &self.settings.font);
        }
        egui::Panel::top("title-bar").exact_size(TITLE_H).frame(plain).show_separator_line(false).show(ui, |ui| {
            title_bar(ui, &t);
            self.browser_toggle(ui, &t);
            self.side_chat_toggle(ui, &t);
        });
        window_edges(ui.ctx());
        geometry::track(ui.ctx());
        // Closing with conversations at work asks first; the offer to
        // resume those paused by the last close.
        self.intercept_close(ui.ctx());
        self.close_dialog(ui.ctx(), &t);
        self.resume_dialog(ui.ctx(), &t);
        self.track_group_pauses();
        // The vault's accounts changed (it connected, or disconnected).
        if api::take_accounts_changed() {
            self.accounts_changed(ui.ctx());
        }
        let ctx = ui.ctx().clone();
        for h in &plugins::parts().hooks {
            h.frame(&ctx, self);
        }
        // A plugin's screen that comes before everything, while it is shut.
        if let Some(gate) = plugins::parts().gates.iter().find(|g| g.shut()) {
            egui::CentralPanel::default().frame(plain).show(ui, |ui| gate.show(ui, self));
            return;
        }
        // Subagents the models deleted since the last frame.
        for id in agent::take_deleted() {
            self.forget(&id);
        }
        egui::Panel::top("tabs").exact_size(TABS_H).frame(plain).show(ui, |ui| self.tab_bar(ui, &t));
        // Subagents the models started since the last frame.
        for (id, s) in agent::take_spawned() {
            self.sessions.insert(id, s);
        }
        self.refresh_account(ui.ctx());
        self.switch_draft();
        self.rename_dialog(ui.ctx(), &t);
        if let Some(r) = cloud::take_started() {
            self.cloud_started(r);
        }
        // Keeps the usage countdowns current.
        ui.ctx().request_repaint_after(Duration::from_secs(30));
        let mut central = |f: &mut dyn FnMut(&mut Ui)| {
            egui::CentralPanel::default().frame(plain).show(ui, |ui| f(ui))
        };
        match self.page {
            Page::Home => { central(&mut |ui| self.home(ui, &t)); }
            Page::Settings => { central(&mut |ui| self.settings_page(ui, &t)); }
            Page::Usage => {
                let mut back = false;
                central(&mut |ui| back = usage_view::page(ui, &t, &mut self.usage));
                if back {
                    self.page = Page::Home;
                }
            }
            Page::Duck => { central(&mut |ui| self.duck_page(ui, &t)); }
            Page::Chats => self.chats_page(ui, &t, plain),
        }
    }
}

impl App {
    /// The chats page: the browser pane right of the chat and its message
    /// box, what's open, and its box at the corner; then what the chat
    /// asked for (a queue action, a revert, a rewind).
    fn chats_page(&mut self, ui: &mut Ui, t: &Theme, plain: egui::Frame) {
        browser::retain(|c| self.settings.tabs.iter().any(|t| t == c) && self.sessions.contains_key(c));
        find::keyboard_target(ui.ctx(), None);
        self.conversation_pane(ui, t);
        // A plugin's tab may be a dashboard: no message box.
        if !matches!(&self.screen, Screen::Chat(id) if plugins::tab_kind(id).is_some_and(|k| !k.composer(id))) {
            egui::Panel::bottom("composer").frame(plain).show(ui, |ui| self.composer(ui, t));
        }
        let central = egui::CentralPanel::default().frame(plain).show(ui, |ui| self.screen_view(ui, t));
        find::keyboard_target(ui.ctx(), None);
        let ctx = ui.ctx().clone();
        self.session_box(&ctx, t, central.response.rect);
        let Screen::Chat(id) = &self.screen else { return };
        let id = id.clone();
        self.chat_actions(&ctx, &id, central.inner);
    }

    pub(crate) fn chat_actions(&mut self, ctx: &egui::Context, id: &str, action: Option<QueueAction>) {
        let revert = ctx.data_mut(|d| d.remove_temp::<(usize, usize, Value)>(Id::unique(REVERT_REQUEST)));
        // Back to before one of the model's items, mid-turn too.
        let rewind = ctx.data_mut(|d| d.remove_temp::<Option<agent::RevertTo>>(Id::unique(REWIND_REQUEST))).flatten();
        if let Some(a) = action {
            self.apply_queue_action(id, a);
        }
        if let Some((msg, block, first)) = revert {
            self.revert(ctx, id, msg, block, first);
        }
        if let (Some(to), Some(s)) = (rewind, self.sessions.get(id)) {
            s.rewind(to);
        }
    }

    /// What the chats page shows: the new screen or the open conversation,
    /// with what its queue asked for.
    fn screen_view(&mut self, ui: &mut Ui, t: &Theme) -> Option<QueueAction> {
        let Screen::Chat(id) = &self.screen else {
            self.new_screen(ui, t);
            return None;
        };
        if let Some(kind) = plugins::tab_kind(id) {
            let id = id.clone();
            kind.show(ui, self, &id);
        } else if cloud::is_key(id) {
            cloud::watch(id, ui.ctx());
            cloud_chat_view(ui, t, id, self.lists.entry(id.clone()).or_default());
        } else if let Some(s) = self.sessions.get(id) {
            let agents_pausing = s.pause_on() && self.agents_pausing(s);
            let list = self.lists.entry(id.clone()).or_default();
            return chat_view(ui, t, id, s, list, &mut self.editing, agents_pausing);
        } else {
            // A conversation that isn't loaded (its file is gone): nothing to draw.
            ui.centered_and_justified(|ui| ui.label("This conversation isn't available."));
        }
        None
    }
}

/// Panics are appended to `path` (crash.log in Oak's folder): a windowed
/// app has no stderr, so a panic would otherwise end Oak without a trace.
/// Each entry has the time, the thread, the message, where, and a
/// backtrace. A worker thread's panic doesn't end Oak by itself, but it
/// poisons the locks it held, and the UI's next `lock().unwrap()` on one
/// panics too; both are logged, the first being the cause.
fn log_panics(path: std::path::PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let message = info.payload_as_str().unwrap_or("(not a string)");
        let at = info.location().map_or("an unknown place".to_string(), |l| l.to_string());
        let text = format!("panicked at {at}:\n{message}\n\n{}", std::backtrace::Backtrace::force_capture());
        append_crash(&path, thread.name().unwrap_or("(unnamed)"), &text);
        default(info);
    }));
}

/// Adds an entry to crash.log (for what isn't a panic: why the GPU device
/// was lost, and what Oak did about it).
pub(crate) fn log_crash(text: &str) {
    append_crash(&store::dir().join("crash.log"), std::thread::current().name().unwrap_or("(unnamed)"), text);
}

fn append_crash(path: &std::path::Path, thread: &str, text: &str) {
    use std::io::Write;
    // Past this size the log starts over, so it can't grow without end.
    const MAX: u64 = 4 << 20;
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let entry = format!(
        "==== {} (unix ms {ms}), Oak {} ({}) (pid {}), thread {thread:?}\n{text}\n\n",
        time::full_time(ms, time::local_offset_ms()),
        update::VERSION,
        update::COMMIT,
        std::process::id(),
    );
    let big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(path) {
        let _ = f.write_all(entry.as_bytes());
    }
}

/// What a plugin's crate names so that it depends on this one, and cargo
/// builds its copies of the crates it shares with Oak exactly as Oak's
/// (`oak_sdk::export_plugin!`, `docs/PLUGINS.md`).
pub const PLUGIN_PIN: &str = env!("CARGO_PKG_VERSION");

/// Oak, from its command line to its last save: what the binary runs.
pub fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--version" {
        println!("oak {} ({})", update::VERSION, update::COMMIT);
        return Ok(());
    }
    #[cfg(windows)]
    if args.len() > 2 && args[1] == "--ctrl-c" {
        tasks::ctrl_c_helper(&args[2..].iter().filter_map(|a| a.parse().ok()).collect::<Vec<_>>());
        return Ok(());
    }
    #[cfg(windows)]
    if args.len() > 1 && args[1] == "--hold-jobs" {
        tasks::hold_jobs(&args[2..]);
        return Ok(());
    }
    if args.len() > 1 && args[1] == "--window" {
        #[cfg(windows)]
        std::process::exit(winctl::main(&args[2..]));
        #[cfg(not(windows))]
        {
            eprintln!("oak --window: the hidden desktop is Windows-only for now. Try the app under Xvfb instead (xvfb-run).");
            std::process::exit(1);
        }
    }
    if args.len() > 1 && args[1] == "--measure-thinking" {
        std::process::exit(measure::main(&args[2..]));
    }
    if args.len() > 1 && args[1] == "--gateway" {
        log_panics(store::dir().join("crash.log"));
        std::process::exit(gateway::main(&args[2..]));
    }
    log_panics(store::dir().join("crash.log"));
    // The plugins, before anything of Oak's runs (`plugins.rs`).
    if args.iter().any(|a| a == "--no-plugins") {
        plugins::disable();
    }
    plugins::load();
    if args.len() > 1 && args[1] == "--plugins" {
        std::process::exit(plugins::report());
    }
    gpu::init();
    let started = std::time::Instant::now();
    tasks::init();
    store::make_private();
    #[cfg(target_os = "linux")]
    install_desktop_entry();
    let viewport = egui::ViewportBuilder::default()
        .with_title("Oak")
        .with_app_id("oak")
        // OAK draws its own title bar (see `title_bar`).
        .with_decorations(false);
    // The size, place and maximized state it was closed with.
    let mut viewport = geometry::restore(viewport);
    // The window's icon where there is one (X11, Windows).
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, wgpu_options: gpu::wgpu_options(), ..Default::default() };
    let run = || eframe::run_native("OAK", options, Box::new(|cc| Ok(Box::new(App::new(cc)))));
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(result) => result,
        // The GPU device was lost: Oak closes as usual and starts again.
        Err(payload) if gpu::is_loss(payload.as_ref()) => {
            gpu::recover(started);
            Ok(())
        }
        Err(payload) => std::panic::resume_unwind(payload),
    };
    // The tasks' records, for the next start (those left running first
    // detached, if the close dialog said so); what else commands left
    // running goes.
    plugins::exiting();
    geometry::save_now();
    tasks::close_all();
    tasks::kill_all();
    // The last saves, on disk before Oak goes.
    store::flush();
    // Freebuff bills a whole hour per session: give it back on the way out.
    freebuff::release();
    // The duck.ai browser, if one was started.
    duck::shutdown();
    // The conversations' browsers, gently, so they keep their cookies.
    browser::shutdown_all();
    // "Restart to update": the new version, now that this one is done.
    update::relaunch_if_asked();
    gpu::relaunch();
    result
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod app_hover_test;
#[cfg(test)]
mod frame_bench;
