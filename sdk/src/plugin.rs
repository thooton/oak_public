//! A plugin and its parts.
//!
//! A plugin implements `Plugin`: who it is (`info`) and what it adds to
//! Oak (`register`, called once when Oak starts, before the window shows).
//! Each part is a trait, implemented by a type of the plugin's and handed
//! to the `Registrar`:
//!
//! - `HomeTab`: a tab in Home's left cell;
//! - `TabKind`: tabs of the plugin's own, by their keys: what they show,
//!   where their messages go, their box in the corner;
//! - `NewKind`: something else the New screen can start;
//! - `CloudBackend`: another service cloud sessions run through, and
//!   `SessionKit`, what a backend's sessions are given (a CLI, files);
//! - `ThemeDef`, `FontDef`: a theme, a font, for Settings to offer;
//! - `Role`: a system prompt;
//! - `Vault`: somewhere else logins and keys are kept;
//! - `SettingsSection`: rows on the Settings page;
//! - `Gate`: a screen that comes before everything (signing in somewhere);
//! - `Hooks`: at startup, every frame, at exit.
//!
//! Parts are `Send + Sync` and take `&self`: a plugin keeps what changes
//! behind its own locks (or in statics; the library is never unloaded).
//! What draws gets the `App`; the rest of Oak is `crate::host()`.
//!
//! Every part goes into Oak wrapped (`Guard`), so a panic in the plugin
//! stops there: Oak is told, and the plugin's parts answer with nothing
//! from then on (`abi::guard`).

use crate::abi::{Health, guard};
use crate::cloud::{Account, Call};
use crate::egui::{self, Rect, Ui};
use crate::host::{App, Response};
use crate::theme::Theme;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Who a plugin is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    /// Its id: lower case, no spaces ("team-server"). Two plugins with
    /// one id don't both load.
    pub id: String,
    /// Its name, as Settings shows it.
    pub name: String,
    pub version: String,
    /// A line about what it does.
    pub about: String,
    /// The ids of the plugins it can't do without: they register first,
    /// and it doesn't load when one is missing.
    pub needs: Vec<String>,
}

impl Info {
    pub fn new(id: &str, name: &str, version: &str, about: &str) -> Info {
        Info { id: id.into(), name: name.into(), version: version.into(), about: about.into(), needs: vec![] }
    }

    pub fn needs(mut self, ids: &[&str]) -> Info {
        self.needs = ids.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// A plugin. `export_plugin!` hands Oak one.
pub trait Plugin: Send + Sync {
    fn info(&self) -> Info;
    /// Hands Oak the plugin's parts. Called once, on the main thread,
    /// before the window shows; `crate::host()` works from here on.
    fn register(&self, r: &mut Registrar);
}

/// What a guarded call answers once its plugin has panicked.
pub trait Fallback {
    fn fallback() -> Self;
}

macro_rules! fallback_default {
    ($($t:ty),* $(,)?) => { $(impl Fallback for $t { fn fallback() -> Self { Default::default() } })* };
}

fallback_default!((), bool, i32, String, Info, TabLook, Value);

impl<T> Fallback for Option<T> {
    fn fallback() -> Self {
        None
    }
}

impl<T> Fallback for Vec<T> {
    fn fallback() -> Self {
        vec![]
    }
}

impl<T> Fallback for Result<T, String> {
    fn fallback() -> Self {
        Err("The plugin behind this has stopped (see Settings).".to_string())
    }
}

impl Fallback for Theme {
    fn fallback() -> Self {
        Theme::oak()
    }
}

impl Fallback for Tags {
    fn fallback() -> Self {
        Tags::of("oak")
    }
}

/// A part of a plugin as Oak holds it: calls go through `abi::guard`.
pub struct Guard<T: ?Sized> {
    health: Arc<Health>,
    inner: Arc<T>,
}

/// Declares a part's trait, and implements it for `Guard<dyn Trait>`,
/// every method guarded.
macro_rules! part {
    (
        $(#[$meta:meta])*
        pub trait $name:ident {
            $(
                $(#[$fmeta:meta])*
                fn $f:ident(&self $(, $a:ident : $t:ty)* $(,)?) $(-> $r:ty)? $($body:block)? $(;)?
            )*
        }
    ) => {
        $(#[$meta])*
        pub trait $name: Send + Sync {
            $( part!(@item [$(#[$fmeta])* fn $f(&self $(, $a: $t)*) $(-> $r)?] $($body)?); )*
        }

        impl $name for Guard<dyn $name> {
            $(
                fn $f(&self $(, $a: $t)*) $(-> $r)? {
                    guard(&self.health, Fallback::fallback, || self.inner.$f($($a),*))
                }
            )*
        }
    };
    (@item [$($sig:tt)*]) => { $($sig)*; };
    (@item [$($sig:tt)*] $body:block) => { $($sig)* $body };
}

part! {
    /// A tab in the heading of Home's left cell, and what the cell shows
    /// under it.
    pub trait HomeTab {
        /// Its id ("notes"): what `App::go_home` takes.
        fn id(&self) -> String;
        fn label(&self) -> String;
        /// A Nerd Font symbol before the label (empty: none).
        fn icon(&self) -> String { String::new() }
        /// The icon is a character of the text font (△), not a symbol.
        fn icon_is_text(&self) -> bool { false }
        /// What hovering it says.
        fn tip(&self) -> String { String::new() }
        /// Where it goes among the tabs: lower first. Oak's Conversations
        /// is 0 and Cloud 10; ties go by label.
        fn order(&self) -> i32 { 100 }
        /// Whether the tab is there at all, asked every frame.
        fn shown(&self) -> bool { true }
        /// Draws the cell: `ui.max_rect()` is all of it.
        fn show(&self, ui: &mut Ui, app: &mut dyn App);
    }
}

/// How a tab looks in the tab strip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabLook {
    pub title: String,
    /// It is working: the tab shows a spinner.
    pub busy: bool,
    /// A Nerd Font symbol before the title (empty: none).
    pub icon: String,
    /// The icon is a character of the text font.
    pub icon_is_text: bool,
}

part! {
    /// Tabs of the plugin's own. A tab is a key (a string the plugin
    /// makes up, "note:7"); Oak keeps the open ones across restarts and
    /// asks the first kind that owns a key for everything about it.
    pub trait TabKind {
        /// Whether `key` is one of this kind's.
        fn owns(&self, key: &str) -> bool;
        /// Whether the tab can still be shown (false: Oak closes it).
        fn exists(&self, _key: &str) -> bool { true }
        fn look(&self, key: &str) -> TabLook;
        /// Draws the tab, over the message box: `ui.max_rect()` is the area.
        fn show(&self, ui: &mut Ui, app: &mut dyn App, key: &str);
        /// Whether the tab has the message box under it.
        fn composer(&self, _key: &str) -> bool { true }
        /// The message box's text and attachments were sent in this tab.
        /// Err: why they couldn't go; Oak says it and leaves them in the box.
        fn send(&self, _app: &mut dyn App, _key: &str, _text: String, _files: Vec<Value>) -> Result<(), String> {
            Err("This tab takes no messages.".to_string())
        }
        /// Draws the tab's box in the top left corner, over the transcript
        /// (`ui::corner_width`, `ui::corner_area`, `ui::panel_box`).
        fn corner(&self, _ctx: &egui::Context, _app: &mut dyn App, _key: &str, _area: Rect) {}
    }
}

part! {
    /// Something the New screen can start besides a conversation here and
    /// a cloud session: a choice in its "where" row.
    pub trait NewKind {
        /// Its id: what `App::go_new` takes.
        fn id(&self) -> String;
        /// The choice's label ("Notebook").
        fn label(&self) -> String;
        fn icon(&self) -> String { String::new() }
        fn tip(&self) -> String { String::new() }
        /// The screen's heading while it is picked ("New notebook").
        fn heading(&self) -> String;
        /// Draws its rows, under the "where" row.
        fn form(&self, ui: &mut Ui, app: &mut dyn App);
        /// The message box was sent with this picked: starts it, with
        /// that as its first message. Err: why not; Oak says it and leaves
        /// the message in the box.
        fn start(&self, app: &mut dyn App, text: String, files: Vec<Value>) -> Result<(), String>;
    }
}

/// The tags a backend's sessions' first messages are wrapped in, which the
/// transcript hides again: `<x-setup>`, `<x-files>`, `<x-continued>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Tags {
    pub setup: (String, String),
    pub files: (String, String),
    pub carry: (String, String),
}

impl Tags {
    /// `<name-setup>`, `<name-files>` and `<name-continued>`.
    pub fn of(name: &str) -> Tags {
        let pair = |what: &str| (format!("<{name}-{what}>"), format!("</{name}-{what}>"));
        Tags { setup: pair("setup"), files: pair("files"), carry: pair("continued") }
    }
}

part! {
    /// What a backend's cloud sessions are given: a server of the
    /// plugin's that a command line tool in the session talks to, which
    /// carries files to and from the user and conversations from one
    /// account to the next. Without one a session is a bare Claude Code
    /// session: no files, and it can't move to another account.
    pub trait SessionKit {
        /// Whether the server can be used now (connected): while not, the
        /// sessions are bare ones, as without a kit.
        fn ready(&self) -> bool { true }
        /// The server's address and the token sessions use it with.
        /// Err: why there is none now ("Not connected to ...").
        fn server(&self) -> Result<(String, String), String>;
        /// What the server is called in sentences to the session and the
        /// user ("the team server").
        fn server_name(&self) -> String;
        /// What the environment is called at Anthropic ("OAK"): a hash of
        /// the script, server and token follows it.
        fn environment_name(&self) -> String;
        /// The environment's setup script (it installs the tool).
        fn setup_script(&self, server: &str) -> String;
        /// The environment's variables, as a JSON object, from the
        /// server's address and token.
        fn variables(&self, server: &str, token: &str) -> Value;
        /// What a new session is told first (in `tags().setup`).
        fn setup_block(&self) -> String;
        /// The tool's name, as the session runs it ("oak").
        fn cli(&self) -> String;
        fn tags(&self) -> Tags;
        /// Other blocks a session's first message may open with, started
        /// by someone else (a scheduled job): (opening tag, closing tag,
        /// the note the transcript shows instead).
        fn notes(&self) -> Vec<(String, String, String)> { vec![] }
        /// A request to the server: `path` from its root, a body with its
        /// content type. Ok: an answer that succeeded; Err names the
        /// status of one that didn't ("HTTP 409"). Oak asks for
        /// `GET /files` (a JSON list of `{name, size, modified}`),
        /// `PUT`, `GET` and `DELETE /files/<name>` (`PUT` answers 409 for
        /// a name in use), and `POST /conversations` (a conversation as
        /// text; the answer's `id` names it for the tool's `resume`).
        fn raw(&self, method: &str, path: &str, body: Option<(Vec<u8>, String)>, timeout: Duration) -> Result<Response, String>;
    }
}

part! {
    /// Another service cloud sessions run through (Oak's own run on this
    /// machine's Claude logins). Oak's engine does the rest: the list, the
    /// stream, the transcript, the box in the corner.
    pub trait CloudBackend {
        /// Its id ("acme"): its accounts' ids start with it and "~".
        fn id(&self) -> String;
        /// Its name, as in "Acme session on a@b.c".
        fn name(&self) -> String;
        fn icon(&self) -> String;
        fn icon_is_text(&self) -> bool { false }
        /// What hovering its choice on the New screen says.
        fn tip(&self) -> String { String::new() }
        /// What the New screen says under it: what a session there can do.
        fn about(&self) -> String { String::new() }
        /// Draws on the New screen under that, while it is picked (a form
        /// to connect with, say).
        fn form(&self, _ui: &mut Ui, _app: &mut dyn App) {}
        /// The accounts sessions can run on now.
        fn accounts(&self) -> Vec<Account>;
        /// Why no session can start now (not connected, no accounts);
        /// None when one can.
        fn unavailable(&self) -> Option<String>;
        /// Account `rid` is out of usage until `until`.
        fn note_limit(&self, rid: &str, until: i64);
        /// Sends a sessions API call on account `rid`'s behalf. Ok: any
        /// answer, error statuses too. Its errors mustn't quote the
        /// answer's body or the request's headers.
        fn request(&self, rid: &str, call: Call) -> Result<Response, String>;
        /// What to say about an answer with error status `status`
        /// (None: Oak's own words, which quote the answer).
        fn problem(&self, _status: u16) -> Option<String> { None }
    }
}

part! {
    /// A theme for Settings to offer beside Oak's.
    pub trait ThemeDef {
        /// Its id, kept in the settings.
        fn id(&self) -> String;
        fn name(&self) -> String;
        fn theme(&self) -> Theme;
    }
}

part! {
    /// A font for the interface's text, for Settings to offer beside IBM
    /// Plex Sans.
    pub trait FontDef {
        /// Its id, kept in the settings.
        fn id(&self) -> String;
        fn name(&self) -> String;
        /// Its regular, bold, italic and bold italic faces (TrueType or
        /// OpenType files), or None when they aren't on this machine yet.
        fn faces(&self) -> Option<Vec<Vec<u8>>>;
        /// Gets the faces onto this machine (a download). Oak calls it off
        /// the UI thread when the font is picked and `faces` has none.
        fn fetch(&self) -> Result<(), String> { Ok(()) }
    }
}

/// A system prompt a conversation can be started with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Role {
    /// Kept in the conversation and the settings.
    pub id: String,
    pub name: String,
    /// The prompt: `{{tools}}` marks where Oak's tool instructions go.
    pub text: String,
    /// The New screen offers it (false: only a plugin starts
    /// conversations with it, `NewConversation::role`).
    pub offered: bool,
    /// A Nerd Font symbol for the tabs of its conversations (empty: none).
    pub icon: String,
}

part! {
    /// Somewhere else logins and keys are kept (a server several machines
    /// share): Oak's accounts then include the vault's, it asks the vault
    /// for their tokens, and new sign-ins go there. One vault at most.
    ///
    /// `kind` is "claude", "codex", "openrouter", "nodeclub" or
    /// "freebuff". What an entry and a login are made of is between Oak's
    /// account code and the vault (`docs/PLUGINS.md` lists the fields).
    pub trait Vault {
        /// What it is called in sentences ("the team server").
        fn name(&self) -> String;
        fn connected(&self) -> bool;
        /// New sign-ins go to the vault (connected, and the user didn't
        /// ask for one on this machine).
        fn takes_new(&self) -> bool;
        /// Whether `error` (one of this vault's) is trouble with the vault
        /// itself, not an answer about one account: Oak then sends the
        /// request through another account.
        fn failed(&self, error: &str) -> bool;
        /// Whether `error` says the vault couldn't be reached at all: Oak
        /// then tries again shortly.
        fn unreachable(&self, error: &str) -> bool;
        /// Whether `error` (from `add`) says the vault can't keep logins
        /// of that kind at all: Oak then keeps the login on this machine,
        /// and says why.
        fn cannot_keep(&self, _error: &str) -> bool { false }
        /// Its entries of `kind`. A vault that has none: an empty list.
        fn list(&self, kind: &str) -> Result<Vec<Value>, String>;
        /// Gives it a login; what it says about it (its id there, whose it is).
        fn add(&self, kind: &str, login: &Value) -> Result<Value, String>;
        /// Takes entry `id` out, for every machine.
        fn remove(&self, kind: &str, id: &str) -> Result<(), String>;
        /// An access token for entry `id` (`{"access_token", "expires_at"}`,
        /// and what else the kind needs), a new one when `refused` (turned
        /// down by the service) is still its current one. Ok(Null): the
        /// vault no longer has the entry.
        fn token(&self, kind: &str, id: &str, refused: Option<&str>) -> Result<Value, String>;
    }
}

part! {
    /// Rows on the Settings page, under a heading.
    pub trait SettingsSection {
        fn title(&self) -> String;
        /// Lower first, after Oak's own rows.
        fn order(&self) -> i32 { 100 }
        fn show(&self, ui: &mut Ui, app: &mut dyn App);
    }
}

part! {
    /// A screen that comes before everything else (signing in to a
    /// server Oak is of no use without).
    pub trait Gate {
        /// Whether the gate is shut now: Oak then shows only `show`.
        fn shut(&self) -> bool;
        fn show(&self, ui: &mut Ui, app: &mut dyn App);
    }
}

part! {
    /// Calls at the turns of Oak's life.
    pub trait Hooks {
        /// The window is up (before its first frame). Start threads here.
        fn started(&self, _ctx: &egui::Context) {}
        /// Every frame, before anything is drawn.
        fn frame(&self, _ctx: &egui::Context, _app: &mut dyn App) {}
        /// Draws at the top of Home's Providers column (a connection's state).
        fn providers_top(&self, _ui: &mut Ui, _app: &mut dyn App) {}
        /// Oak is closing.
        fn exiting(&self) {}
    }
}

/// Everything plugins registered, as Oak takes it.
#[derive(Default)]
pub struct Parts {
    pub home_tabs: Vec<Arc<dyn HomeTab>>,
    pub tab_kinds: Vec<Arc<dyn TabKind>>,
    pub new_kinds: Vec<Arc<dyn NewKind>>,
    pub cloud_backends: Vec<Arc<dyn CloudBackend>>,
    /// What cloud sessions are given, by backend id ("": Oak's own).
    pub session_kits: Vec<(String, Arc<dyn SessionKit>)>,
    pub themes: Vec<Arc<dyn ThemeDef>>,
    pub fonts: Vec<Arc<dyn FontDef>>,
    pub roles: Vec<Role>,
    pub vault: Option<Arc<dyn Vault>>,
    pub settings: Vec<Arc<dyn SettingsSection>>,
    pub gates: Vec<Arc<dyn Gate>>,
    pub hooks: Vec<Arc<dyn Hooks>>,
}

/// Where a plugin puts its parts (`Plugin::register`).
pub struct Registrar {
    health: Arc<Health>,
    parts: Parts,
}

macro_rules! adders {
    ($($(#[$meta:meta])* $f:ident, $field:ident, $tr:ident;)*) => {
        $(
            $(#[$meta])*
            pub fn $f(&mut self, part: impl $tr + 'static) {
                let part: Arc<dyn $tr> = Arc::new(part);
                self.parts.$field.push(Arc::new(Guard { health: self.health.clone(), inner: part }));
            }
        )*
    };
}

impl Registrar {
    /// An empty one (Oak makes it).
    pub fn new() -> Registrar {
        Registrar { health: Arc::new(Health::default()), parts: Parts::default() }
    }

    /// What was registered.
    pub fn into_parts(self) -> Parts {
        self.parts
    }

    adders! {
        /// A tab on Home.
        home_tab, home_tabs, HomeTab;
        /// Tabs of the plugin's own.
        tab_kind, tab_kinds, TabKind;
        /// A choice on the New screen.
        new_kind, new_kinds, NewKind;
        /// A service cloud sessions run through.
        cloud_backend, cloud_backends, CloudBackend;
        /// A theme.
        theme, themes, ThemeDef;
        /// A font.
        font, fonts, FontDef;
        /// Rows on Settings.
        settings, settings, SettingsSection;
        /// A screen before everything.
        gate, gates, Gate;
        /// Calls at startup, every frame, at exit.
        hooks, hooks, Hooks;
    }

    /// A system prompt.
    pub fn role(&mut self, role: Role) {
        self.parts.roles.push(role);
    }

    /// What the cloud sessions of backend `backend` are given ("": Oak's
    /// own, on this machine's and the vault's Claude logins).
    pub fn session_kit(&mut self, backend: &str, kit: impl SessionKit + 'static) {
        let part: Arc<dyn SessionKit> = Arc::new(kit);
        self.parts.session_kits.push((backend.to_string(), Arc::new(Guard { health: self.health.clone(), inner: part })));
    }

    /// Where logins are kept besides this machine (the last one registered).
    pub fn vault(&mut self, vault: impl Vault + 'static) {
        let part: Arc<dyn Vault> = Arc::new(vault);
        self.parts.vault = Some(Arc::new(Guard { health: self.health.clone(), inner: part }));
    }
}

impl Default for Registrar {
    fn default() -> Self {
        Registrar::new()
    }
}

/// A plugin as Oak holds it: its own calls guarded, and its parts given
/// its health (`abi::start` wraps the plugin in this).
pub struct Guarded {
    health: Arc<Health>,
    inner: Box<dyn Plugin>,
}

impl Guarded {
    pub fn new(inner: Box<dyn Plugin>) -> Guarded {
        let health = Arc::new(Health::default());
        let g = Guarded { health, inner };
        let info = g.info();
        g.health.set_name(if info.name.is_empty() { &info.id } else { &info.name });
        g
    }
}

impl Plugin for Guarded {
    fn info(&self) -> Info {
        guard(&self.health, Info::default, || self.inner.info())
    }

    fn register(&self, r: &mut Registrar) {
        // The plugin's parts share its health: one panic stops them all.
        let mut mine = Registrar { health: self.health.clone(), parts: Parts::default() };
        guard(&self.health, || (), || self.inner.register(&mut mine));
        if self.health.dead() {
            return;
        }
        let (p, q) = (&mut r.parts, mine.parts);
        p.home_tabs.extend(q.home_tabs);
        p.tab_kinds.extend(q.tab_kinds);
        p.new_kinds.extend(q.new_kinds);
        p.cloud_backends.extend(q.cloud_backends);
        p.themes.extend(q.themes);
        p.fonts.extend(q.fonts);
        p.roles.extend(q.roles);
        p.settings.extend(q.settings);
        p.gates.extend(q.gates);
        p.hooks.extend(q.hooks);
        p.session_kits.extend(q.session_kits);
        if q.vault.is_some() {
            p.vault = q.vault;
        }
    }
}
