//! Plugins: the dynamic libraries Oak loads when it starts, what they
//! registered, and Oak as they reach it (`oak_sdk::Host`).
//!
//! A plugin is a library (`.so`, `.dll`, `.dylib`) in one of `dirs()`: the
//! `plugins` folder in Oak's folder, the one beside the executable, and
//! the folders `OAK_PLUGINS` names. `load` opens each, reads its stamp
//! (`oak_sdk::abi`), and starts those built exactly as this Oak was; the
//! others are listed on Settings with what differs. A started plugin hands
//! over its parts (`oak_sdk::Parts`), which the rest of Oak asks for here:
//! `parts()`. Libraries are never unloaded.
//!
//! `OAK_PLUGINS=off` (or `--no-plugins`) loads none.

use crate::{api, store};
use oak_sdk::abi::{ENTRY_SYMBOL, Entry, Link, Loaded, MAGIC, Stamp};
use oak_sdk::{Info, Parts, Plugin, Registrar, Request, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A library Oak looked at.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    /// Who it is, once it said (a library that doesn't fit never does).
    pub(crate) info: Option<Info>,
    /// Why it isn't running: it doesn't fit, it failed to start, or it
    /// panicked since.
    pub(crate) trouble: Option<String>,
}

static PARTS: OnceLock<Parts> = OnceLock::new();
static FOUND: Mutex<Vec<Found>> = Mutex::new(Vec::new());
static OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the plugins registered (nothing, before `load` or with none).
pub(crate) fn parts() -> &'static Parts {
    static NONE: OnceLock<Parts> = OnceLock::new();
    PARTS.get().unwrap_or_else(|| NONE.get_or_init(Parts::default))
}

/// The libraries looked at, in the order they were.
pub(crate) fn found() -> Vec<Found> {
    FOUND.lock().unwrap().clone()
}

/// `--no-plugins`: `load` loads none.
pub fn disable() {
    OFF.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The folders plugins are looked for in.
pub(crate) fn dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Some(list) = std::env::var_os("OAK_PLUGINS") {
        dirs.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs.push(store::dir().join("plugins"));
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("plugins"))) {
        dirs.push(beside);
    }
    let mut seen = vec![];
    dirs.retain(|d| {
        let real = d.canonicalize().unwrap_or_else(|_| d.clone());
        let new = !seen.contains(&real);
        seen.push(real);
        new
    });
    dirs
}

/// Whether `path` is named as a dynamic library is on this platform.
fn is_library(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(std::env::consts::DLL_EXTENSION))
}

/// Opens the library at `path` and starts its plugin. Err: why it can't
/// run in this Oak.
fn open(path: &Path) -> Result<Box<dyn Plugin>, String> {
    // SAFETY: loading a library runs its initializers; that is what the
    // user asked for by putting it in a plugins folder.
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("it couldn't be loaded: {e}"))?;
    // Never unloaded: what it registers, its threads and what it leaves in
    // egui's memory point into it.
    let lib: &'static libloading::Library = Box::leak(Box::new(lib));
    // SAFETY: the symbol's type is the one `export_plugin!` exports; a
    // library with another function of that name is caught by the magic.
    let entry = unsafe { lib.get::<unsafe extern "C" fn() -> *const Entry>(ENTRY_SYMBOL) }.map_err(|_| "it isn't an Oak plugin (it exports no oak_plugin_entry)".to_string())?;
    // SAFETY: as above.
    let entry = unsafe { entry() };
    if entry.is_null() {
        return Err("it isn't an Oak plugin (its entry is empty)".into());
    }
    // SAFETY: `magic` and `size` are the first fields of a `#[repr(C)]`
    // struct, read before anything else is trusted.
    let (magic, size) = unsafe { ((*entry).magic, (*entry).size) };
    if magic != MAGIC {
        return Err("it isn't an Oak plugin (its entry isn't one)".into());
    }
    if size as usize != size_of::<Entry>() {
        return Err("it was built against another version of oak-sdk; build it against this Oak's source".into());
    }
    // SAFETY: an `Entry` of this layout, from a library that stays loaded.
    let entry: &'static Entry = unsafe { &*entry };
    if let Some(why) = Stamp::current().mismatch(&entry.stamp) {
        return Err(why);
    }
    let link = Link { host: host(), parking: parking_lot_core::oak_parking_state() };
    let mut out = Loaded::default();
    // SAFETY: the stamps are equal: the library was built from the same
    // source, with the same compiler, crates and options as this Oak.
    let ok = unsafe { (entry.init)(&link, &mut out) };
    match (ok, out.plugin) {
        (true, Some(p)) => Ok(p),
        _ => Err(if out.error.is_empty() { "it failed to start".into() } else { out.error }),
    }
}

/// Loads every plugin and takes what they register. Once, at startup,
/// before the window and before any thread of Oak's waits on a lock.
pub fn load() {
    oak_sdk::abi::set_host(host());
    if PARTS.get().is_some() {
        return;
    }
    let off = OFF.load(std::sync::atomic::Ordering::Relaxed) || std::env::var("OAK_PLUGINS").is_ok_and(|v| v == "off" || v == "0");
    let mut found: Vec<Found> = vec![];
    let mut started: Vec<(usize, Info, Box<dyn Plugin>)> = vec![];
    for dir in if off { vec![] } else { dirs() } {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_library(p)).collect();
        files.sort();
        for path in files {
            let at = found.len();
            match open(&path) {
                Ok(plugin) => {
                    let info = plugin.info();
                    let trouble = if info.id.is_empty() {
                        Some("it has no id".to_string())
                    } else if started.iter().any(|(_, i, _)| i.id == info.id) {
                        Some(format!("a plugin with the id {} is loaded already", info.id))
                    } else {
                        None
                    };
                    found.push(Found { path, info: Some(info.clone()), trouble: trouble.clone() });
                    if trouble.is_none() {
                        started.push((at, info, plugin));
                    }
                }
                Err(why) => found.push(Found { path, info: None, trouble: Some(why) }),
            }
        }
    }
    // Those whose needs are met register, the needed first; the rest say
    // what they miss.
    let mut registrar = Registrar::new();
    let mut done: Vec<String> = vec![];
    while let Some(i) = started.iter().position(|(_, info, _)| info.needs.iter().all(|n| done.contains(n))) {
        let (at, info, plugin) = started.remove(i);
        plugin.register(&mut registrar);
        // (A panic while registering was caught in the plugin, which told
        // `plugin_failed`; FOUND isn't filled yet, so it is noted here.)
        if let Some(why) = FAILED.lock().unwrap().remove(&name_of(&info)) {
            found[at].trouble = Some(why);
        } else {
            done.push(info.id.clone());
        }
        // It stays for good: its parts point into it.
        Box::leak(plugin);
    }
    for (at, info, plugin) in started {
        let missing: Vec<&str> = info.needs.iter().filter(|n| !done.contains(n)).map(|s| s.as_str()).collect();
        found[at].trouble = Some(format!("it needs the plugin{} {}, which {} loaded", oak_sdk::plural(missing.len()), missing.join(", "), if missing.len() == 1 { "isn't" } else { "aren't" }));
        Box::leak(plugin);
    }
    *FOUND.lock().unwrap() = found;
    let _ = PARTS.set(registrar.into_parts());
    crate::agent::prompt::add_roles(&parts().roles);
}

/// `oak --plugins`: says where plugins are looked for, which were found
/// and which of them run. 1 when one doesn't.
pub fn report() -> i32 {
    println!("Oak {} looks for plugins in:", crate::update::VERSION);
    for d in dirs() {
        println!("  {}{}", d.display(), if d.is_dir() { "" } else { "  (not there)" });
    }
    let found = found();
    if found.is_empty() {
        println!("None found.");
    }
    for f in &found {
        let who = f.info.as_ref().map_or_else(|| "?".to_string(), |i| format!("{} {} ({})", name_of(i), i.version, i.id));
        match &f.trouble {
            None => println!("ok       {who}\n         {}", f.path.display()),
            Some(why) => println!("NOT RUN  {who}\n         {}\n         {why}", f.path.display()),
        }
    }
    let p = parts();
    println!(
        "Registered: {} Home tab{}, {} tab kind{}, {} New kind{}, {} cloud backend{}, {} session kit{}, {} theme{}, {} font{}, {} role{}, {} settings section{}, {} gate{}, {} hook set{}, {}.",
        p.home_tabs.len(), oak_sdk::plural(p.home_tabs.len()),
        p.tab_kinds.len(), oak_sdk::plural(p.tab_kinds.len()),
        p.new_kinds.len(), oak_sdk::plural(p.new_kinds.len()),
        p.cloud_backends.len(), oak_sdk::plural(p.cloud_backends.len()),
        p.session_kits.len(), oak_sdk::plural(p.session_kits.len()),
        p.themes.len(), oak_sdk::plural(p.themes.len()),
        p.fonts.len(), oak_sdk::plural(p.fonts.len()),
        p.roles.len(), oak_sdk::plural(p.roles.len()),
        p.settings.len(), oak_sdk::plural(p.settings.len()),
        p.gates.len(), oak_sdk::plural(p.gates.len()),
        p.hooks.len(), oak_sdk::plural(p.hooks.len()),
        if p.vault.is_some() { "a vault" } else { "no vault" },
    );
    found.iter().any(|f| f.trouble.is_some()) as i32
}

/// What a plugin's health is named by (`oak_sdk::plugin::Guarded`).
fn name_of(info: &Info) -> String {
    if info.name.is_empty() { info.id.clone() } else { info.name.clone() }
}

/// Plugins that panicked before `FOUND` was filled, by name.
static FAILED: std::sync::LazyLock<Mutex<HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

/// The window's context, for `Host::repaint` (set when the window is up).
static CONTEXT: OnceLock<eframe::egui::Context> = OnceLock::new();

/// The window is up: plugins' `started` hooks run.
pub(crate) fn started(ctx: &eframe::egui::Context) {
    let _ = CONTEXT.set(ctx.clone());
    for h in &parts().hooks {
        h.started(ctx);
    }
}

/// Oak is closing.
pub(crate) fn exiting() {
    for h in &parts().hooks {
        h.exiting();
    }
}

/// The plugin tab kind that owns `key`, if one does.
pub(crate) fn tab_kind(key: &str) -> Option<&'static std::sync::Arc<dyn oak_sdk::TabKind>> {
    parts().tab_kinds.iter().find(|k| k.owns(key))
}

/// The vault, when a plugin brought one.
pub(crate) fn vault() -> Option<&'static std::sync::Arc<dyn oak_sdk::Vault>> {
    parts().vault.as_ref()
}

/// Oak, as plugins reach it.
struct OakHost;

/// The one `Host`.
pub(crate) fn host() -> &'static dyn oak_sdk::Host {
    static HOST: OakHost = OakHost;
    &HOST
}

/// What plugins tell each other (`Host::shared`).
static SHARED: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

/// Oak's HTTP client for a plugin's request: its own, or one like it that
/// answers a redirect instead of following it (`redirects` false), or
/// goes around the proxy (`direct`).
fn http_client(redirects: bool, direct: bool) -> reqwest::blocking::Client {
    static CLIENTS: [OnceLock<reqwest::blocking::Client>; 4] = [const { OnceLock::new() }; 4];
    if redirects && !direct {
        return api::http();
    }
    CLIENTS[redirects as usize * 2 + direct as usize]
        .get_or_init(|| {
            let mut b = reqwest::blocking::Client::builder()
                .timeout(None)
                .connect_timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(60))
                .tcp_keepalive(Duration::from_secs(30));
            if !redirects {
                b = b.redirect(reqwest::redirect::Policy::none());
            }
            if direct {
                b = b.no_proxy();
            }
            b.build().unwrap()
        })
        .clone()
}

/// A reqwest answer as the SDK's: its body is read as it arrives.
pub(crate) fn response(resp: reqwest::blocking::Response) -> Response {
    let headers = resp.headers().iter().map(|(n, v)| (n.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
    Response { status: resp.status().as_u16(), headers, body: Box::new(resp) }
}

impl oak_sdk::Host for OakHost {
    fn version(&self) -> String {
        crate::update::VERSION.to_string()
    }

    fn dir(&self) -> PathBuf {
        store::dir()
    }

    fn log_crash(&self, text: &str) {
        crate::log_crash(text);
    }

    fn plugin_failed(&self, plugin: &str, why: &str) {
        let line = format!("it stopped: {why} (crash.log in Oak's folder has more)");
        let mut found = FOUND.lock().unwrap();
        match found.iter_mut().find(|f| f.info.as_ref().is_some_and(|i| name_of(i) == plugin)) {
            Some(f) => f.trouble = Some(line),
            None => {
                FAILED.lock().unwrap().insert(plugin.to_string(), line);
            }
        }
        drop(found);
        self.repaint();
    }

    fn repaint(&self) {
        if let Some(ctx) = CONTEXT.get() {
            ctx.request_repaint();
        }
    }

    fn repaint_after(&self, after: Duration) {
        if let Some(ctx) = CONTEXT.get() {
            ctx.request_repaint_after(after);
        }
    }

    fn context(&self) -> Option<eframe::egui::Context> {
        CONTEXT.get().cloned()
    }

    fn http(&self, request: Request) -> Result<Response, String> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| format!("{} isn't an HTTP method", request.method))?;
        let client = http_client(request.redirects, request.direct);
        let build = || {
            let mut req = client.request(method.clone(), &request.url).timeout(request.timeout);
            for (name, value) in &request.headers {
                req = req.header(name, value);
            }
            match &request.body {
                Some(b) => req.body(b.clone()),
                None => req,
            }
        };
        crate::provider::send(build).map(response).map_err(|e| api::error_chain(&e))
    }

    fn write_private(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        store::write_private(path, data).map_err(|e| e.to_string())
    }

    fn uuid(&self) -> String {
        api::new_uuid()
    }

    fn shared(&self, name: &str) -> Option<Value> {
        SHARED.lock().unwrap().as_ref().and_then(|m| m.get(name).cloned())
    }

    fn share(&self, name: &str, value: Option<Value>) {
        let mut shared = SHARED.lock().unwrap();
        let map = shared.get_or_insert_with(HashMap::new);
        match value {
            Some(v) => {
                map.insert(name.to_string(), v);
            }
            None => {
                map.remove(name);
            }
        }
    }

    fn signed_in(&self) -> bool {
        api::signed_in()
    }

    fn vault_changed(&self) {
        std::thread::spawn(|| {
            api::vault_changed();
            host().repaint();
        });
    }

    fn usage_local(&self, f: &mut dyn FnMut(&mut dyn Iterator<Item = &oak_sdk::usage::Rec>)) -> Result<(), String> {
        crate::usage::local_records(|records| f(records))
    }

    fn usage_set_shared(&self, source: &str, records: Vec<oak_sdk::usage::Rec>) {
        crate::usage::set_remote(source.to_string(), records);
    }

    fn usage_add_shared(&self, source: &str, records: &[oak_sdk::usage::Rec]) -> bool {
        crate::usage::add_remote(source, records)
    }

    fn usage_show(&self, source: Option<&str>, label: &str, detail: &str) {
        crate::usage::show_shared(source, label, detail);
    }

    fn usage_discard_before(&self, cutoff: i64) -> Result<(), String> {
        crate::usage::discard_before(cutoff)
    }

    fn cloud_name(&self, key: &str, title: &str) {
        crate::cloud::name(key, title);
    }

    fn cloud_forget(&self, backend: &str) {
        crate::cloud::forget(backend);
    }

    fn cloud_starting(&self) -> bool {
        crate::cloud::starting()
    }

    fn cloud_accounts(&self) -> Vec<oak_sdk::cloud::Account> {
        api::cloud_accounts()
    }

    fn image_block(&self, bytes: &[u8]) -> Result<Value, String> {
        crate::images::from_bytes(bytes).map(|i| i.block())
    }

    fn file_block(&self, path: &Path, bytes: &[u8]) -> Result<Value, String> {
        crate::transcript::file_block(path, bytes)
    }

    fn markdown(&self, ui: &mut eframe::egui::Ui, theme: &oak_sdk::Theme, text: &str, key: (usize, usize), highlighted: bool) {
        if highlighted {
            crate::transcript::markdown_highlighted(ui, theme, text, key);
        } else {
            crate::transcript::markdown(ui, theme, text, key);
        }
    }

    fn message_bar(&self, ui: &mut eframe::egui::Ui, theme: &oak_sdk::Theme, msg: eframe::egui::Rect, id: eframe::egui::Id, copy_text: &dyn Fn() -> String, at: Option<i64>) {
        crate::transcript::message_bar(ui, theme, msg, id, copy_text, at, None);
    }

    fn image(&self, ui: &mut eframe::egui::Ui, block: &Value, max_h: f32) -> Option<eframe::egui::Response> {
        crate::images::show(ui, block, max_h)
    }

    fn copy_menu(&self, ui: &mut eframe::egui::Ui, id: eframe::egui::Id, rect: eframe::egui::Rect, message: Option<&dyn Fn() -> String>, extra: Option<&dyn Fn(&mut eframe::egui::Ui)>) {
        crate::copy_menu::show(ui, id, rect, message, extra);
    }

    fn chat_scroll(&self, ui: &mut eframe::egui::Ui, theme: &oak_sdk::Theme, id: eframe::egui::Id, gap: f32, f: &mut dyn FnMut(&mut eframe::egui::Ui)) {
        crate::composer::chat_scroll(ui, theme, id, gap, |ui| f(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{Headless, click, painted, rect_of};
    use crate::{Page, Screen};
    use eframe::egui::PointerButton::Primary;

    /// The example plugin, loaded into the whole app, headless: its tab on
    /// Home draws (with its own copy of egui, into Oak's window) and takes
    /// clicks, its theme and its row are on Settings, its role on the New
    /// screen. Needs the plugin built, and no other test in the process
    /// (plugins are loaded once, for all of it):
    /// `cargo build -p oak-hello && OAK_PLUGINS=target/debug cargo test --lib the_example_plugin -- --ignored`
    #[test]
    #[ignore]
    fn the_example_plugin_draws_in_the_window() {
        let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built example plugin");
        // (Absolute: the scratch OAK folder moves nothing, but be sure.)
        let dirs: Vec<PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
        unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
        load();
        let found = found();
        let hello = found.iter().find(|f| f.info.as_ref().is_some_and(|i| i.id == "hello")).unwrap_or_else(|| panic!("the example plugin wasn't found: {found:?}"));
        assert_eq!(hello.trouble, None);
        assert_eq!(crate::agent::prompt::role("haiku").name, "Haiku");

        let (dir, mut h, mut app) = Headless::start("plugin", &[]);
        let ctx = h.ctx.clone();
        let mut frames = |app: &mut crate::App, events: Vec<eframe::egui::Event>| {
            let mut out = h.events(app, events);
            for _ in 0..2 {
                out = h.events(app, vec![]);
            }
            painted(&out)
        };
        // Home has its tab; picked, the plugin draws the cell.
        app.page = Page::Home;
        let shown = frames(&mut app, vec![]);
        let tab = rect_of(&shown, "Hello");
        let shown = frames(&mut app, click(tab.center(), Primary));
        assert_eq!(app.home_list, "hello");
        let texts = |shown: &[(String, eframe::egui::Rect)]| shown.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().join("|");
        assert!(texts(&shown).contains("Hello from a plugin") && texts(&shown).contains("clicked 0 times"), "{}", texts(&shown));
        // A click in it reaches the plugin's widget.
        let shown = frames(&mut app, click(rect_of(&shown, "Count").center(), Primary));
        assert!(texts(&shown).contains("clicked 1 times"), "{}", texts(&shown));
        // What it does to the window goes through `oak_sdk::App`.
        let shown = frames(&mut app, click(rect_of(&shown, "New conversation").center(), Primary));
        assert!(matches!(app.screen, Screen::New) && app.page == Page::Chats);
        assert!(texts(&shown).contains("Haiku"), "its role is offered: {}", texts(&shown));

        // Settings: its theme, its row, and itself among the plugins.
        app.page = Page::Settings;
        let shown = frames(&mut app, vec![]);
        let all = texts(&shown);
        for want in ["Moss", "A row the example plugin adds.", "Hello 0.1.0"] {
            assert!(all.contains(want), "{want} isn't on Settings: {all}");
        }
        let before = ctx.global_style().visuals.panel_fill;
        frames(&mut app, click(rect_of(&shown, "Moss").center(), Primary));
        assert_eq!(app.settings.theme, "moss");
        let after = ctx.global_style().visuals.panel_fill;
        assert_ne!(before, after, "the theme is applied");
        // Its row's button goes to its tab.
        app.home_list = String::new();
        let shown = frames(&mut app, vec![]);
        frames(&mut app, click(rect_of(&shown, "Open its tab").center(), Primary));
        assert!(app.page == Page::Home && app.home_list == "hello");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Whatever plugins `OAK_PLUGINS` holds, loaded into the whole app,
    /// headless: each loads, and each part that draws does so without
    /// panicking (every Home tab, every theme, Settings, every choice on
    /// the New screen, and the tabs `OAK_PLUGIN_TABS` names, separated by
    /// commas). For a plugin's own repository to run against its build:
    /// `OAK_PLUGINS=<folder> cargo test --lib every_plugin_draws -- --ignored`
    #[test]
    #[ignore]
    fn every_plugin_draws() {
        let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built plugins");
        let dirs: Vec<PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
        unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
        load();
        let troubles = || found().iter().filter_map(|f| f.trouble.as_ref().map(|t| format!("{}: {t}", f.path.display()))).collect::<Vec<_>>();
        assert!(!found().is_empty(), "no plugin was found");
        assert_eq!(troubles(), Vec::<String>::new());

        let (dir, mut h, mut app) = Headless::start("plugins", &[]);
        let ctx = h.ctx.clone();
        // `OAK_PLUGIN_WAIT_MS`: how long each view is given to fetch what it
        // shows (plugins that talk to a server); `OAK_PLUGIN_SHOW=1` prints
        // the text each view drew.
        let wait = std::env::var("OAK_PLUGIN_WAIT_MS").ok().and_then(|ms| ms.parse().ok()).unwrap_or(0);
        let show = std::env::var_os("OAK_PLUGIN_SHOW").is_some();
        let mut frames = |app: &mut crate::App, what: &str| {
            let mut out = h.events(app, vec![]);
            for round in 0..2 {
                if round == 1 && wait > 0 {
                    std::thread::sleep(Duration::from_millis(wait));
                }
                for _ in 0..3 {
                    out = h.events(app, vec![]);
                }
            }
            if show {
                let texts: Vec<String> = painted(&out).into_iter().map(|(s, _)| s.replace('\n', " ")).filter(|s| !s.trim().is_empty()).collect();
                println!("== {what}\n{}", texts.join(" | "));
            }
            assert_eq!(troubles(), Vec::<String>::new(), "after drawing {what}");
        };
        let p = parts();
        frames(&mut app, "the first screen");
        for theme in std::iter::once(crate::theme::OAK.to_string()).chain(p.themes.iter().map(|t| t.id())).rev() {
            app.settings.theme = theme.clone();
            crate::theme::apply_theme(&ctx, &crate::theme::theme(&theme));
            for tab in &p.home_tabs {
                app.page = Page::Home;
                app.home_list = tab.id();
                frames(&mut app, &format!("Home's {} tab in the {theme} theme", tab.id()));
            }
        }
        app.page = Page::Settings;
        frames(&mut app, "Settings");
        let kinds = p.new_kinds.iter().map(|k| k.id()).chain(p.cloud_backends.iter().map(|b| b.id()));
        for kind in kinds {
            oak_sdk::App::go_new(&mut app, &kind);
            frames(&mut app, &format!("the New screen's {kind}"));
        }
        for key in std::env::var("OAK_PLUGIN_TABS").unwrap_or_default().split(',').map(str::trim).filter(|k| !k.is_empty()) {
            assert!(tab_kind(key).is_some(), "no plugin has the tab {key}");
            app.open(key.to_string());
            assert!(matches!(&app.screen, Screen::Chat(k) if k == key), "the tab {key} didn't open");
            frames(&mut app, &format!("the tab {key}"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
