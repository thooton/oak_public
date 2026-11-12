//! Oak, as a plugin reaches it.
//!
//! `Host` is the whole of Oak any thread may use: its folder, HTTP, the
//! usage log, cloud sessions, what plugins share with each other, and the
//! parts of Oak's own interface a plugin draws with (markdown, pictures, a
//! chat's scrolling column). `crate::host()` gives it, once the plugin was
//! started.
//!
//! `App` is the window while a plugin draws in it: its tabs, the message
//! box, the hint over it. It comes as an argument to what draws
//! (`HomeTab::show`, `TabKind::show`, ...), on the UI thread, and isn't
//! kept.
//!
//! Nothing here names a type of Oak's other dependencies (no HTTP
//! client's, no async runtime's): what crosses is std, serde_json, egui,
//! and what this crate defines.

use crate::chat::Chat;
use crate::cloud::Account;
use crate::egui::{self, Id, Rect, Ui};
use crate::theme::Theme;
use crate::usage::Rec;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An HTTP request for `Host::http`.
#[derive(Clone, Debug)]
pub struct Request {
    /// "GET", "POST", ...
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Bounds the whole request, its body's download included.
    pub timeout: Duration,
    /// Follow redirects.
    pub redirects: bool,
    /// Connect straight to the address, whatever proxy is set (a service
    /// on this machine).
    pub direct: bool,
}

impl Request {
    pub fn new(method: &str, url: impl Into<String>) -> Request {
        Request { method: method.to_string(), url: url.into(), headers: vec![], body: None, timeout: Duration::from_secs(30), redirects: true, direct: false }
    }

    pub fn get(url: impl Into<String>) -> Request {
        Request::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Request {
        Request::new("POST", url)
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// `Authorization: Bearer`.
    pub fn bearer(self, token: &str) -> Request {
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// A redirect is the answer, not followed.
    pub fn no_redirects(mut self) -> Request {
        self.redirects = false;
        self
    }

    /// No proxy: straight to the address (a service on this machine).
    pub fn direct(mut self) -> Request {
        self.direct = true;
        self
    }

    /// A JSON body.
    pub fn json(mut self, body: &Value) -> Request {
        self.body = Some(serde_json::to_vec(body).unwrap_or_default());
        self.header("Content-Type", "application/json")
    }

    /// A body of `kind` (its content type).
    pub fn body(mut self, bytes: Vec<u8>, kind: &str) -> Request {
        self.body = Some(bytes);
        self.header("Content-Type", kind)
    }

    pub fn timeout(mut self, timeout: Duration) -> Request {
        self.timeout = timeout;
        self
    }

    /// Sends it (`Host::http`).
    pub fn send(self) -> Result<Response, String> {
        crate::host().http(self)
    }
}

/// An HTTP answer: its status and headers, and its body as it arrives
/// (an event stream is read line by line while it is open).
pub struct Response {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl Response {
    /// 200 to 299.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The whole body.
    pub fn bytes(mut self) -> Result<Vec<u8>, String> {
        let mut out = vec![];
        self.body.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// The whole body, as text (what isn't UTF-8 replaced).
    pub fn text(self) -> Result<String, String> {
        self.bytes().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// The whole body, as JSON.
    pub fn json(self) -> Result<Value, String> {
        serde_json::from_slice(&self.bytes()?).map_err(|e| e.to_string())
    }
}

/// A conversation of this machine's, as a plugin's list shows it.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    /// Its tab's key.
    pub id: String,
    pub title: String,
    pub folder: String,
    pub model: String,
    /// The role (system prompt) it was started with.
    pub role: String,
    /// When its last message was (ms since 1970).
    pub at: i64,
    pub busy: bool,
}

/// A conversation to start on this machine (`App::start_conversation`):
/// in the folder and with the model and effort the New screen's rows show
/// (`App::local_form`).
#[derive(Clone, Debug, Default)]
pub struct NewConversation {
    /// Its title (empty: from its first message).
    pub title: String,
    /// The role (system prompt) it runs with: a `plugin::Role`'s id, or
    /// one of Oak's (empty: the one picked in Settings).
    pub role: String,
    /// Its first message (empty with no files: it waits for one).
    pub text: String,
    /// The message's attachments, as the message box holds them.
    pub files: Vec<Value>,
}

/// Oak's services, for any thread.
pub trait Host: Send + Sync {
    /// Oak's version ("0.2.214").
    fn version(&self) -> String;
    /// Oak's folder (settings, logins, conversations). A plugin keeps its
    /// files there, under names of its own.
    fn dir(&self) -> PathBuf;
    /// Appends `text` to crash.log.
    fn log_crash(&self, text: &str);
    /// A plugin's part panicked (`abi::guard`): Oak stops calling the
    /// plugin and says so on Settings.
    fn plugin_failed(&self, plugin: &str, why: &str);
    /// Draw the window again soon: after a background job changed what it
    /// shows.
    fn repaint(&self);
    fn repaint_after(&self, after: Duration);
    /// The window's egui context (None before the window is up).
    fn context(&self) -> Option<egui::Context>;

    /// Sends an HTTP request with Oak's client (its proxy settings, TLS
    /// roots and user agent). Err: it couldn't be sent or no answer came;
    /// an answer with an error status is Ok.
    fn http(&self, request: Request) -> Result<Response, String>;
    /// Writes a file only its owner can read (a login, a token), whole or
    /// not at all.
    fn write_private(&self, path: &Path, data: &[u8]) -> Result<(), String>;
    /// A random UUID.
    fn uuid(&self) -> String;

    /// What plugins tell each other: a value under a name (a server's
    /// connection under its plugin's id, say). None: nothing
    /// was put there.
    fn shared(&self, name: &str) -> Option<Value>;
    /// Puts `value` under `name` (None takes it away).
    fn share(&self, name: &str, value: Option<Value>);

    /// Whether any login or key is on this machine (or a vault is
    /// connected): something a conversation could run on.
    fn signed_in(&self) -> bool;
    /// The vault (`plugin::Vault`) connected or disconnected, or its
    /// logins changed: Oak brings its accounts in step, off this thread.
    fn vault_changed(&self);

    /// Works with this machine's usage records, oldest first (the others'
    /// aren't in them). Err: the log couldn't be saved first.
    fn usage_local(&self, f: &mut dyn FnMut(&mut dyn Iterator<Item = &Rec>)) -> Result<(), String>;
    /// The records other machines made, from `source` (where they are
    /// shared: an address). They show while `usage_show` names it.
    fn usage_set_shared(&self, source: &str, records: Vec<Rec>);
    /// More of `source`'s records. False when Oak holds another source's.
    fn usage_add_shared(&self, source: &str, records: &[Rec]) -> bool;
    /// Which source's shared records the Usage page shows with this
    /// machine's (None: this machine's only), and what it says about the
    /// sharing (a label, and a line of detail).
    fn usage_show(&self, source: Option<&str>, label: &str, detail: &str);
    /// Drops every record from before `cutoff` (ms since 1970), on disk
    /// too: the shared log was reset then.
    fn usage_discard_before(&self, cutoff: i64) -> Result<(), String>;

    /// Names the cloud session behind tab `key` before it is opened (one
    /// started elsewhere, which no list has named yet).
    fn cloud_name(&self, key: &str, title: &str);
    /// Forgets what Oak holds of backend `id`'s sessions (it disconnected,
    /// or connects to another server).
    fn cloud_forget(&self, backend: &str);
    /// A cloud session is being started.
    fn cloud_starting(&self) -> bool;
    /// The accounts Oak's own cloud sessions run on.
    fn cloud_accounts(&self) -> Vec<Account>;

    /// A picture's bytes as an image block (scaled to what the API takes).
    fn image_block(&self, bytes: &[u8]) -> Result<Value, String>;
    /// A file's bytes as the block the message box would attach.
    fn file_block(&self, path: &Path, bytes: &[u8]) -> Result<Value, String>;

    // What follows draws: call it from the UI thread, while drawing.

    /// Markdown, as a transcript shows it, selectable; `key` tells this
    /// text from the others on the page. `highlighted`: its code blocks
    /// colored for their language.
    fn markdown(&self, ui: &mut Ui, theme: &Theme, text: &str, key: (usize, usize), highlighted: bool);
    /// The bar under a message (`msg`, its rectangle): Copy, and when it
    /// was sent.
    fn message_bar(&self, ui: &mut Ui, theme: &Theme, msg: Rect, id: Id, copy_text: &dyn Fn() -> String, at: Option<i64>);
    /// An image block, at most `max_h` tall; a click opens it large.
    fn image(&self, ui: &mut Ui, block: &Value, max_h: f32) -> Option<egui::Response>;
    /// The right-click menu of a message in `rect`: Copy (the selection,
    /// or `message`), and what `extra` adds.
    fn copy_menu(&self, ui: &mut Ui, id: Id, rect: Rect, message: Option<&dyn Fn() -> String>, extra: Option<&dyn Fn(&mut Ui)>);
    /// A chat drawn as widgets in the conversation's column, kept scrolled
    /// to the newest: `f` lays it out top down, its rows `gap` apart.
    fn chat_scroll(&self, ui: &mut Ui, theme: &Theme, id: Id, gap: f32, f: &mut dyn FnMut(&mut Ui));
}

/// The window, while a plugin draws in it.
pub trait App {
    /// The theme in use.
    fn theme(&self) -> Theme;
    /// Opens (or shows) the tab `key`: a conversation's id, a cloud
    /// session's key, or a key one of the plugin's `TabKind`s owns.
    fn open(&mut self, key: &str);
    /// Closes the tabs `gone` says to, and forgets them as tabs to reopen.
    fn close_tabs(&mut self, gone: &dyn Fn(&str) -> bool);
    /// The key of the tab shown, when one is (not Home, not the New screen).
    fn shown(&self) -> Option<String>;
    /// Shows Home, on tab `tab` (a `HomeTab`'s id, "local", "cloud").
    fn go_home(&mut self, tab: &str);
    /// Shows the New screen with `kind` picked (a `NewKind`'s id, a cloud
    /// backend's id, "local").
    fn go_new(&mut self, kind: &str);
    /// Says `text` over the message box, in the error color (None clears it).
    fn hint(&mut self, text: Option<String>);
    /// The message box's text.
    fn input(&mut self) -> &mut String;
    /// What is attached to the message (image and file blocks).
    fn attachments(&mut self) -> &mut Vec<Value>;
    /// This machine's conversations (not side chats, not subagents).
    fn conversations(&self) -> Vec<Conversation>;
    /// Starts a conversation on this machine and opens its tab. Err: why
    /// it couldn't (the folder is missing, say), also shown as the hint.
    fn start_conversation(&mut self, new: NewConversation) -> Result<String, String>;
    /// The New screen's rows for a conversation on this machine: the
    /// folder, the model and the effort.
    fn local_form(&mut self, ui: &mut Ui);
    /// Draws a transcript the plugin has, as a conversation's is drawn,
    /// read-only. `key` is its tab; `version` changes when the transcript
    /// does (`make` is called only then); `tail` draws what follows the
    /// last message, and `tail_sig` changes when that would draw otherwise.
    fn transcript(&mut self, ui: &mut Ui, key: &str, version: u64, make: &mut dyn FnMut() -> Chat, tail_sig: u64, tail: &mut dyn FnMut(&mut Ui));
    /// The list of backend `id`'s cloud sessions, as Home's Cloud tab
    /// shows Oak's own: a click opens the session.
    fn cloud_list(&mut self, ui: &mut Ui, backend: &str);
}
