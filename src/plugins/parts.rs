//! What plugins registered, as the rest of Oak asks for it.
//!
//! A plugin hands Oak descriptors (`oak_sdk::abi`: texts, a `user`
//! pointer, C functions). Each kind is kept here as a struct whose methods
//! are what Oak asks of it: they call the plugin's functions, with the
//! window lent to the plugin while it draws (`api::with_app`), and turn
//! the JSON its named calls answer into Oak's types.

use super::api::{self, text};
use super::{Response, take_answer};
use crate::App;
use eframe::egui::{self, Rect, Ui};
use oak_sdk::abi::*;
use oak_sdk::cloud::{Account, Call};
pub(crate) use oak_sdk::plugin::{Info, Role, TabLook, Tags};
use oak_sdk::rt::{bytes, bytes_from, parts_from, s};
use oak_sdk::theme::{Palette, Theme};
use serde_json::{Value, json};
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

/// A part's own pointer. Plugins' parts are called from any thread
/// (`docs/PLUGINS.md` says so to who writes one).
#[derive(Clone, Copy)]
pub(crate) struct User(pub(crate) *mut c_void);

unsafe impl Send for User {}
unsafe impl Sync for User {}

/// A part's named calls (`OakCall`).
#[derive(Clone, Copy)]
struct Calls {
    call: Option<OakCall>,
    user: User,
}

/// What a named call answered.
enum Answer {
    /// The part has no such method.
    Unknown,
    Ok(i64, Vec<u8>),
    Failed(String),
}

impl Calls {
    fn ask_with(&self, method: &str, args: Value, data: &[u8]) -> Answer {
        let Some(call) = self.call else { return Answer::Unknown };
        let args = if args.is_null() { String::new() } else { args.to_string() };
        // SAFETY: the plugin gave this function and pointer for this.
        let (code, out) = bytes_from(|out, user| unsafe { call(self.user.0, s(method), s(&args), bytes(data), out, user) });
        match code {
            OAK_CALL_UNKNOWN => Answer::Unknown,
            n if n < 0 => Answer::Failed(String::from_utf8_lossy(&out).into_owned()),
            n => Answer::Ok(n, out),
        }
    }

    fn ask(&self, method: &str, args: Value) -> Answer {
        self.ask_with(method, args, &[])
    }

    /// A yes or no (`default` from a part without the method, or one that failed).
    fn yes(&self, method: &str, args: Value, default: bool) -> bool {
        match self.ask(method, args) {
            Answer::Ok(n, _) => n != 0,
            _ => default,
        }
    }

    /// Text (empty from a part without the method).
    fn text(&self, method: &str, args: Value) -> String {
        match self.ask(method, args) {
            Answer::Ok(_, out) => String::from_utf8_lossy(&out).into_owned(),
            _ => String::new(),
        }
    }

    /// JSON. Err: why the call failed, or that the part can't do it.
    fn json(&self, method: &str, args: Value) -> Result<Value, String> {
        match self.ask(method, args) {
            Answer::Ok(_, out) if out.is_empty() => Ok(Value::Null),
            Answer::Ok(_, out) => serde_json::from_slice(&out).map_err(|e| format!("the plugin's answer to {method} isn't JSON: {e}")),
            Answer::Failed(why) => Err(why),
            Answer::Unknown => Err(format!("The plugin can't do that ({method}).")),
        }
    }

    /// Nothing to answer. Err: why it failed.
    fn done(&self, method: &str, args: Value, without: &str) -> Result<(), String> {
        match self.ask(method, args) {
            Answer::Ok(..) => Ok(()),
            Answer::Failed(why) => Err(why),
            Answer::Unknown => Err(without.to_string()),
        }
    }

    /// An HTTP answer, by its handle.
    fn answer(&self, method: &str, args: Value, data: &[u8]) -> Result<Response, String> {
        match self.ask_with(method, args, data) {
            Answer::Ok(handle, _) => take_answer(handle as u64).ok_or_else(|| "the plugin gave no answer".to_string()),
            Answer::Failed(why) => Err(why),
            Answer::Unknown => Err(format!("The plugin can't do that ({method}).")),
        }
    }
}

/// A tab on Home.
pub(crate) struct HomeTab {
    id: String,
    label: String,
    icon: String,
    icon_is_text: bool,
    /// A link at the row's right end that opens the tab `id`.
    link: bool,
    /// With `link`: it opens a page of its own, which `show` draws.
    page: bool,
    tip: String,
    order: i32,
    shown: Option<OakAsk>,
    show: Option<OakBody>,
    user: User,
}

impl HomeTab {
    /// # Safety
    /// `d` is a descriptor a plugin handed over, valid for this call.
    pub(crate) unsafe fn new(d: &OakHomeTab) -> HomeTab {
        unsafe { HomeTab { id: text(d.id), label: text(d.label), icon: text(d.icon), icon_is_text: d.flags & OAK_ICON_IS_TEXT != 0, link: d.flags & OAK_HOME_TAB_LINK != 0, page: d.flags & OAK_HOME_TAB_LINK != 0 && d.flags & OAK_HOME_TAB_PAGE != 0, tip: text(d.tip), order: d.order, shown: d.shown, show: d.show, user: User(d.user) } }
    }

    pub(crate) fn id(&self) -> String {
        self.id.clone()
    }

    pub(crate) fn label(&self) -> String {
        self.label.clone()
    }

    pub(crate) fn icon(&self) -> String {
        self.icon.clone()
    }

    pub(crate) fn icon_is_text(&self) -> bool {
        self.icon_is_text
    }

    pub(crate) fn tip(&self) -> String {
        self.tip.clone()
    }

    pub(crate) fn link(&self) -> bool {
        self.link
    }

    /// A link to a page of the plugin's own (`Page::Plugin`), not to a tab.
    pub(crate) fn page(&self) -> bool {
        self.page
    }

    pub(crate) fn order(&self) -> i32 {
        self.order
    }

    pub(crate) fn shown(&self) -> bool {
        // SAFETY: the plugin's own function and pointer.
        self.shown.is_none_or(|f| unsafe { f(self.user.0) })
    }

    pub(crate) fn show(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.show, self.user, ui, app);
    }
}

/// Tabs of a plugin's own.
pub(crate) struct TabKind {
    owns: Option<OakAskKey>,
    show: Option<OakDrawKey>,
    corner: Option<OakCorner>,
    calls: Calls,
    user: User,
}

impl TabKind {
    pub(crate) fn new(d: &OakTabKind) -> TabKind {
        TabKind { owns: d.owns, show: d.show, corner: d.corner, calls: Calls { call: d.call, user: User(d.user) }, user: User(d.user) }
    }

    pub(crate) fn owns(&self, key: &str) -> bool {
        // SAFETY: the plugin's own function and pointer.
        self.owns.is_some_and(|f| unsafe { f(self.user.0, s(key)) })
    }

    pub(crate) fn exists(&self, key: &str) -> bool {
        self.calls.yes("exists", json!({"key": key}), true)
    }

    pub(crate) fn look(&self, key: &str) -> TabLook {
        let v = self.calls.json("look", json!({"key": key})).unwrap_or_default();
        let field = |name: &str| v[name].as_str().unwrap_or("").to_string();
        TabLook { title: field("title"), busy: v["busy"].as_bool().unwrap_or(false), icon: field("icon"), icon_is_text: v["icon_is_text"].as_bool().unwrap_or(false) }
    }

    pub(crate) fn show(&self, ui: &mut Ui, app: &mut App, key: &str) {
        let Some(show) = self.show else { return };
        // SAFETY: the plugin's own function and pointer; `ui` lives through the call.
        api::with_app(app, || unsafe { show(self.user.0, api::ui_ptr(ui), s(key)) });
    }

    pub(crate) fn composer(&self, key: &str) -> bool {
        self.calls.yes("composer", json!({"key": key}), true)
    }

    pub(crate) fn send(&self, app: &mut App, key: &str, text: String, files: Vec<Value>) -> Result<(), String> {
        api::with_app(app, || self.calls.done("send", json!({"key": key, "text": text, "files": files}), "This tab takes no messages."))
    }

    pub(crate) fn corner(&self, _ctx: &egui::Context, app: &mut App, key: &str, area: Rect) {
        let Some(corner) = self.corner else { return };
        // SAFETY: as in `show`.
        api::with_app(app, || unsafe { corner(self.user.0, s(key), &api::rect_out(area)) });
    }
}

/// A choice on the New screen.
pub(crate) struct NewKind {
    id: String,
    label: String,
    icon: String,
    tip: String,
    heading: String,
    form: Option<OakBody>,
    calls: Calls,
    user: User,
}

impl NewKind {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakNewKind) -> NewKind {
        unsafe { NewKind { id: text(d.id), label: text(d.label), icon: text(d.icon), tip: text(d.tip), heading: text(d.heading), form: d.form, calls: Calls { call: d.call, user: User(d.user) }, user: User(d.user) } }
    }

    pub(crate) fn id(&self) -> String {
        self.id.clone()
    }

    pub(crate) fn label(&self) -> String {
        self.label.clone()
    }

    pub(crate) fn icon(&self) -> String {
        self.icon.clone()
    }

    pub(crate) fn tip(&self) -> String {
        self.tip.clone()
    }

    pub(crate) fn heading(&self) -> String {
        self.heading.clone()
    }

    pub(crate) fn form(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.form, self.user, ui, app);
    }

    pub(crate) fn start(&self, app: &mut App, text: String, files: Vec<Value>) -> Result<(), String> {
        api::with_app(app, || self.calls.done("start", json!({"text": text, "files": files}), "The plugin can't start that."))
    }
}

/// Another service cloud sessions run through.
pub(crate) struct CloudBackend {
    id: String,
    name: String,
    icon: String,
    icon_is_text: bool,
    tip: String,
    about: String,
    form: Option<OakBody>,
    calls: Calls,
    user: User,
}

impl CloudBackend {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakCloudBackend) -> CloudBackend {
        unsafe {
            CloudBackend {
                id: text(d.id),
                name: text(d.name),
                icon: text(d.icon),
                icon_is_text: d.flags & OAK_ICON_IS_TEXT != 0,
                tip: text(d.tip),
                about: text(d.about),
                form: d.form,
                calls: Calls { call: d.call, user: User(d.user) },
                user: User(d.user),
            }
        }
    }

    pub(crate) fn id(&self) -> String {
        self.id.clone()
    }

    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }

    pub(crate) fn icon(&self) -> String {
        self.icon.clone()
    }

    pub(crate) fn icon_is_text(&self) -> bool {
        self.icon_is_text
    }

    pub(crate) fn tip(&self) -> String {
        self.tip.clone()
    }

    pub(crate) fn about(&self) -> String {
        self.about.clone()
    }

    pub(crate) fn form(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.form, self.user, ui, app);
    }

    pub(crate) fn accounts(&self) -> Vec<Account> {
        self.calls.json("accounts", Value::Null).ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    pub(crate) fn unavailable(&self) -> Option<String> {
        match self.calls.ask("unavailable", Value::Null) {
            Answer::Ok(_, out) => Some(String::from_utf8_lossy(&out).into_owned()).filter(|why| !why.is_empty()),
            Answer::Failed(why) => Some(why),
            Answer::Unknown => None,
        }
    }

    pub(crate) fn note_limit(&self, rid: &str, until: i64) {
        self.calls.ask("note_limit", json!({"rid": rid, "until": until}));
    }

    pub(crate) fn request(&self, rid: &str, call: Call) -> Result<Response, String> {
        self.calls.answer("request", call.to_json(rid), &[])
    }

    pub(crate) fn problem(&self, status: u16) -> Option<String> {
        Some(self.calls.text("problem", json!({"status": status}))).filter(|p| !p.is_empty())
    }
}

/// What a backend's cloud sessions are given.
pub(crate) struct SessionKit {
    calls: Calls,
}

impl SessionKit {
    pub(crate) fn new(d: &OakSessionKit) -> SessionKit {
        SessionKit { calls: Calls { call: d.call, user: User(d.user) } }
    }

    pub(crate) fn ready(&self) -> bool {
        self.calls.yes("ready", Value::Null, true)
    }

    pub(crate) fn server(&self) -> Result<(String, String), String> {
        let v = self.calls.json("server", Value::Null)?;
        Ok((v["url"].as_str().unwrap_or("").to_string(), v["token"].as_str().unwrap_or("").to_string()))
    }

    pub(crate) fn server_name(&self) -> String {
        self.calls.text("server_name", Value::Null)
    }

    pub(crate) fn environment_name(&self) -> String {
        self.calls.text("environment_name", Value::Null)
    }

    pub(crate) fn setup_script(&self, server: &str) -> String {
        self.calls.text("setup_script", json!({"server": server}))
    }

    pub(crate) fn variables(&self, server: &str, token: &str) -> Value {
        self.calls.json("variables", json!({"server": server, "token": token})).unwrap_or_else(|_| json!({}))
    }

    pub(crate) fn setup_block(&self) -> String {
        self.calls.text("setup_block", Value::Null)
    }

    pub(crate) fn cli(&self) -> String {
        self.calls.text("cli", Value::Null)
    }

    pub(crate) fn tags(&self) -> Tags {
        let v = self.calls.json("tags", Value::Null).unwrap_or_default();
        let pair = |name: &str, or: (String, String)| match (v[name][0].as_str(), v[name][1].as_str()) {
            (Some(open), Some(close)) => (open.to_string(), close.to_string()),
            _ => or,
        };
        let oak = Tags::of("oak");
        Tags { setup: pair("setup", oak.setup), files: pair("files", oak.files), carry: pair("carry", oak.carry) }
    }

    pub(crate) fn notes(&self) -> Vec<(String, String, String)> {
        let v = self.calls.json("notes", Value::Null).unwrap_or_default();
        let field = |n: &Value, i: usize| n[i].as_str().unwrap_or("").to_string();
        v.as_array().map(|list| list.iter().map(|n| (field(n, 0), field(n, 1), field(n, 2))).collect()).unwrap_or_default()
    }

    pub(crate) fn raw(&self, method: &str, path: &str, body: Option<(Vec<u8>, String)>, timeout: Duration) -> Result<Response, String> {
        let (data, kind) = match body {
            Some((data, kind)) => (data, Value::from(kind)),
            None => (vec![], Value::Null),
        };
        self.calls.answer("raw", json!({"method": method, "path": path, "content_type": kind, "timeout_ms": timeout.as_millis() as u64}), &data)
    }
}

/// A theme a plugin adds.
pub(crate) struct ThemeDef {
    id: String,
    name: String,
    palette: Palette,
}

impl ThemeDef {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakThemeDef) -> ThemeDef {
        let palette = Palette { bg: d.bg, panel: d.panel, panel2: d.panel2, ink: d.ink, ink2: d.ink2, ink3: d.ink3, ink4: d.ink4, accent: d.accent, error: d.error, line_alpha: d.line_alpha.min(255) as u8 };
        unsafe { ThemeDef { id: text(d.id), name: text(d.name), palette } }
    }

    pub(crate) fn id(&self) -> String {
        self.id.clone()
    }

    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }

    pub(crate) fn theme(&self) -> Theme {
        Theme::from_palette(self.palette)
    }
}

/// A font a plugin adds.
pub(crate) struct FontDef {
    id: String,
    name: String,
    calls: Calls,
}

impl FontDef {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakFontDef) -> FontDef {
        unsafe { FontDef { id: text(d.id), name: text(d.name), calls: Calls { call: d.call, user: User(d.user) } } }
    }

    pub(crate) fn id(&self) -> String {
        self.id.clone()
    }

    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }

    /// Its regular, bold, italic and bold italic faces, when they are on
    /// this machine.
    pub(crate) fn faces(&self) -> Option<Vec<Vec<u8>>> {
        let call = self.calls.call?;
        // SAFETY: the plugin's own function and pointer.
        let (n, faces) = parts_from(|out, user| unsafe { call(self.calls.user.0, s("faces"), s(""), bytes(&[]), out, user) });
        (n == 4 && faces.len() == 4).then_some(faces)
    }

    pub(crate) fn fetch(&self) -> Result<(), String> {
        match self.calls.ask("fetch", Value::Null) {
            Answer::Failed(why) => Err(why),
            _ => Ok(()),
        }
    }
}

/// Somewhere else logins and keys are kept.
pub(crate) struct Vault {
    calls: Calls,
}

impl Vault {
    pub(crate) fn new(d: &OakVault) -> Vault {
        Vault { calls: Calls { call: d.call, user: User(d.user) } }
    }

    pub(crate) fn name(&self) -> String {
        self.calls.text("name", Value::Null)
    }

    pub(crate) fn connected(&self) -> bool {
        self.calls.yes("connected", Value::Null, false)
    }

    pub(crate) fn takes_new(&self) -> bool {
        self.calls.yes("takes_new", Value::Null, false)
    }

    pub(crate) fn failed(&self, error: &str) -> bool {
        self.calls.yes("failed", json!({"error": error}), false)
    }

    pub(crate) fn unreachable(&self, error: &str) -> bool {
        self.calls.yes("unreachable", json!({"error": error}), false)
    }

    pub(crate) fn cannot_keep(&self, error: &str) -> bool {
        self.calls.yes("cannot_keep", json!({"error": error}), false)
    }

    pub(crate) fn list(&self, kind: &str) -> Result<Vec<Value>, String> {
        Ok(self.calls.json("list", json!({"kind": kind}))?.as_array().cloned().unwrap_or_default())
    }

    pub(crate) fn add(&self, kind: &str, login: &Value) -> Result<Value, String> {
        self.calls.json("add", json!({"kind": kind, "login": login}))
    }

    pub(crate) fn remove(&self, kind: &str, id: &str) -> Result<(), String> {
        self.calls.done("remove", json!({"kind": kind, "id": id}), "The vault can't remove logins.")
    }

    pub(crate) fn token(&self, kind: &str, id: &str, refused: Option<&str>) -> Result<Value, String> {
        self.calls.json("token", json!({"kind": kind, "id": id, "refused": refused}))
    }
}

/// Rows on a plugin's settings page (`App::plugin_settings_page`).
pub(crate) struct SettingsSection {
    title: String,
    order: i32,
    show: Option<OakBody>,
    user: User,
}

impl SettingsSection {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakSettings) -> SettingsSection {
        unsafe { SettingsSection { title: text(d.title), order: d.order, show: d.show, user: User(d.user) } }
    }

    pub(crate) fn title(&self) -> String {
        self.title.clone()
    }

    pub(crate) fn order(&self) -> i32 {
        self.order
    }

    pub(crate) fn show(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.show, self.user, ui, app);
    }
}

/// A screen that comes before everything else.
pub(crate) struct Gate {
    shut: Option<OakAsk>,
    show: Option<OakBody>,
    user: User,
}

impl Gate {
    pub(crate) fn new(d: &OakGate) -> Gate {
        Gate { shut: d.shut, show: d.show, user: User(d.user) }
    }

    pub(crate) fn shut(&self) -> bool {
        // SAFETY: the plugin's own function and pointer.
        self.shut.is_some_and(|f| unsafe { f(self.user.0) })
    }

    pub(crate) fn show(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.show, self.user, ui, app);
    }
}

/// Calls at the turns of Oak's life.
pub(crate) struct Hooks {
    started: Option<OakDo>,
    frame: Option<OakDo>,
    providers_top: Option<OakBody>,
    exiting: Option<OakDo>,
    composer_corner: Option<OakBody>,
    home_scene: Option<OakBody>,
    chat_scene: Option<OakBody>,
    composer_button: Option<OakBody>,
    user: User,
}

impl Hooks {
    pub(crate) fn new(d: &OakHooks) -> Hooks {
        Hooks { started: d.started, frame: d.frame, providers_top: d.providers_top, exiting: d.exiting, composer_corner: d.composer_corner, home_scene: d.home_scene, chat_scene: d.chat_scene, composer_button: d.composer_button, user: User(d.user) }
    }

    pub(crate) fn started(&self, _ctx: &egui::Context) {
        if let Some(f) = self.started {
            // SAFETY: the plugin's own function and pointer.
            unsafe { f(self.user.0) };
        }
    }

    pub(crate) fn frame(&self, _ctx: &egui::Context, app: &mut App) {
        if let Some(f) = self.frame {
            // SAFETY: as above.
            api::with_app(app, || unsafe { f(self.user.0) });
        }
    }

    pub(crate) fn providers_top(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.providers_top, self.user, ui, app);
    }

    pub(crate) fn composer_corner(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.composer_corner, self.user, ui, app);
    }

    /// Whether it draws above a message box's Settings link.
    pub(crate) fn has_composer_corner(&self) -> bool {
        self.composer_corner.is_some()
    }

    pub(crate) fn composer_button(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.composer_button, self.user, ui, app);
    }

    /// Whether it may draw a button in a message box's top right corner.
    pub(crate) fn has_composer_button(&self) -> bool {
        self.composer_button.is_some()
    }

    pub(crate) fn home_scene(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.home_scene, self.user, ui, app);
    }

    /// Whether it draws over transcripts (`chat_scene`).
    pub(crate) fn has_chat_scene(&self) -> bool {
        self.chat_scene.is_some()
    }

    pub(crate) fn chat_scene(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.chat_scene, self.user, ui, app);
    }

    pub(crate) fn exiting(&self) {
        if let Some(f) = self.exiting {
            // SAFETY: as above.
            unsafe { f(self.user.0) };
        }
    }
}

/// What transcripts show in place of the model's words (`crate::rewrites`).
pub(crate) struct Rewriter {
    key: &'static str,
    rewrites: Option<OakBits>,
    rewrite: Option<OakRewrite>,
    calls: Calls,
    user: User,
}

impl Rewriter {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakRewriter) -> Rewriter {
        // (Kept for good, as the parts are: `parts()` lends them for good.)
        let key: &'static str = Box::leak(unsafe { text(d.key) }.into_boxed_str());
        Rewriter { key, rewrites: d.rewrites, rewrite: d.rewrite, calls: Calls { call: d.call, user: User(d.user) }, user: User(d.user) }
    }
}

impl crate::rewrites::Rewriting for Rewriter {
    fn key(&self) -> &'static str {
        self.key
    }

    fn bits(&self) -> u32 {
        // SAFETY: the plugin gave this function and pointer for this.
        self.rewrites.map_or(0, |f| unsafe { f(self.user.0) })
    }

    fn epoch(&self) -> u32 {
        match self.calls.ask("epoch", Value::Null) {
            Answer::Ok(n, _) => n as u32,
            _ => 0,
        }
    }

    fn sections(&self, text: &str, done: bool) -> Option<Vec<(usize, usize)>> {
        match self.calls.ask_with("sections", json!({"done": done}), text.as_bytes()) {
            Answer::Ok(_, out) => {
                let list: Vec<(usize, usize)> = serde_json::from_slice(&out).ok()?;
                // (Ranges that don't fit the text go: what the plugin got wrong.)
                Some(list.into_iter().filter(|&(a, b)| a < b && text.get(a..b).is_some()).collect())
            }
            _ => None,
        }
    }

    fn rewrite(&self, text: &str, progress: &mut dyn FnMut(&str)) -> (Result<String, String>, f64) {
        let Some(f) = self.rewrite else { return (Err("The rewriter can't rewrite.".into()), 0.0) };
        unsafe extern "C" fn take(user: *mut c_void, text: OakStr) {
            // SAFETY: `user` is the `progress` below, for this call.
            let progress = unsafe { &mut *(user as *mut &mut dyn FnMut(&str)) };
            progress(unsafe { oak_sdk::rt::str_of(text) });
        }
        let mut progress = progress;
        let p = &mut progress as *mut &mut dyn FnMut(&str) as *mut c_void;
        let mut cost = 0.0f64;
        // SAFETY: as above; `cost` and the sink live through the call.
        let (ok, out) = bytes_from(|out, user| unsafe { f(self.user.0, s(text), take, p, out, user, &mut cost) });
        let said = String::from_utf8_lossy(&out).into_owned();
        (if ok { Ok(said) } else { Err(said) }, if cost.is_finite() { cost } else { 0.0 })
    }
}

/// A box beside the message box.
pub(crate) struct ComposerBox {
    show: Option<OakBody>,
    user: User,
}

impl ComposerBox {
    pub(crate) fn new(d: &OakComposerBox) -> ComposerBox {
        ComposerBox { show: d.show, user: User(d.user) }
    }

    pub(crate) fn show(&self, ui: &mut Ui, app: &mut App) {
        api::draw(self.show, self.user, ui, app);
    }
}

/// Models a plugin serves conversations with (`crate::provided`).
pub(crate) struct Provider {
    pub(crate) id: String,
    pub(crate) name: String,
    stream: Option<OakProvide>,
    calls: Calls,
    user: User,
}

impl Provider {
    /// # Safety
    /// As `HomeTab::new`.
    pub(crate) unsafe fn new(d: &OakProvider) -> Provider {
        Provider { id: unsafe { text(d.id) }, name: unsafe { text(d.name) }, stream: d.stream, calls: Calls { call: d.call, user: User(d.user) }, user: User(d.user) }
    }

    /// Its models now, as it lists them (JSON objects; none when it
    /// answers nothing that reads).
    pub(crate) fn models(&self) -> Vec<Value> {
        match self.calls.ask("models", Value::Null) {
            Answer::Ok(_, out) => serde_json::from_slice::<Vec<Value>>(&out).unwrap_or_default(),
            _ => vec![],
        }
    }

    /// Its logo, a PNG, if it has one.
    pub(crate) fn logo(&self) -> Option<Vec<u8>> {
        match self.calls.ask("logo", Value::Null) {
            Answer::Ok(_, out) if !out.is_empty() => Some(out),
            _ => None,
        }
    }

    /// Streams the reply to `request` (JSON), each event to `event` (false
    /// from it: stop). Err: why it failed.
    pub(crate) fn stream(&self, request: &Value, event: &mut dyn FnMut(&str) -> bool) -> Result<(), String> {
        let Some(f) = self.stream else { return Err(format!("{} can't answer.", self.name)) };
        unsafe extern "C" fn emit(user: *mut c_void, text: OakStr) -> bool {
            // SAFETY: `user` is the `event` below, for this call.
            let event = unsafe { &mut *(user as *mut &mut dyn FnMut(&str) -> bool) };
            event(unsafe { oak_sdk::rt::str_of(text) })
        }
        let mut event = event;
        let e = &mut event as *mut &mut dyn FnMut(&str) -> bool as *mut c_void;
        let json = request.to_string();
        // SAFETY: the plugin's function and pointer; the sink and `event`
        // live through the call.
        let (ok, out) = bytes_from(|out, user| unsafe { f(self.user.0, s(&json), emit, e, out, user) });
        if ok { Ok(()) } else { Err(String::from_utf8_lossy(&out).into_owned()) }
    }
}

/// Everything plugins registered (or one of them did: `plugins::Running`).
#[derive(Clone, Default)]
pub(crate) struct Parts {
    pub(crate) home_tabs: Vec<Arc<HomeTab>>,
    pub(crate) tab_kinds: Vec<Arc<TabKind>>,
    pub(crate) new_kinds: Vec<Arc<NewKind>>,
    pub(crate) cloud_backends: Vec<Arc<CloudBackend>>,
    /// By the id of the backend each is for ("": Oak's own sessions).
    pub(crate) session_kits: Vec<(String, Arc<SessionKit>)>,
    pub(crate) themes: Vec<Arc<ThemeDef>>,
    pub(crate) fonts: Vec<Arc<FontDef>>,
    pub(crate) roles: Vec<Role>,
    pub(crate) vault: Option<Arc<Vault>>,
    /// By the id of the plugin that registered each (its settings page).
    pub(crate) settings: Vec<(String, Arc<SettingsSection>)>,
    pub(crate) gates: Vec<Arc<Gate>>,
    pub(crate) hooks: Vec<Arc<Hooks>>,
    pub(crate) rewriters: Vec<Arc<Rewriter>>,
    pub(crate) composer_boxes: Vec<Arc<ComposerBox>>,
    pub(crate) providers: Vec<Arc<Provider>>,
}

impl Parts {
    /// Adds what one plugin registered, after what the others did.
    pub(crate) fn absorb(&mut self, more: Parts) {
        self.home_tabs.extend(more.home_tabs);
        self.tab_kinds.extend(more.tab_kinds);
        self.new_kinds.extend(more.new_kinds);
        self.cloud_backends.extend(more.cloud_backends);
        self.session_kits.extend(more.session_kits);
        self.themes.extend(more.themes);
        self.fonts.extend(more.fonts);
        self.roles.extend(more.roles);
        self.vault = more.vault.or(self.vault.take());
        self.settings.extend(more.settings);
        self.gates.extend(more.gates);
        self.hooks.extend(more.hooks);
        self.rewriters.extend(more.rewriters);
        self.composer_boxes.extend(more.composer_boxes);
        // (One provider per id: the first.)
        for p in more.providers {
            if !self.providers.iter().any(|q| q.id == p.id) {
                self.providers.push(p);
            }
        }
    }

    /// The settings sections of the plugin `id`, in their order.
    pub(crate) fn settings_of(&self, id: &str) -> Vec<Arc<SettingsSection>> {
        let mut of: Vec<_> = self.settings.iter().filter(|(p, _)| p == id).map(|(_, s)| s.clone()).collect();
        of.sort_by_key(|s| s.order());
        of
    }
}
