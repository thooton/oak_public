// Conversations and settings, as JSON files under %APPDATA%\oak on Windows,
// ~/.config/oak on Linux (or under OAK_DIR when set).

use crate::tools::{Head, one_line};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub fn dir() -> PathBuf {
    let d = match std::env::var_os("OAK_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("oak"),
    };
    let _ = std::fs::create_dir_all(d.join("conversations"));
    d
}

/// Writes a file so that a crash or power cut leaves either the old content
/// or the new, never half of it: to a temporary file beside it, then renamed
/// over it. `private` makes it readable by its owner only (mode 0600 on
/// Unix, as the vault keeps its logins).
fn write_replacing(path: &std::path::Path, data: &[u8], private: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let tmp = path.with_file_name(name);
    let written = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        }
        let _ = private;
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()
    })();
    match written.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Writes a file whole or not at all (see `write_replacing`).
pub fn write_atomic(path: &std::path::Path, data: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_replacing(path, data.as_ref(), false)
}

/// Writes a file only its owner can read, whole or not at all: the vault's token, access tokens and keys.
pub fn write_private(path: &std::path::Path, data: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_replacing(path, data.as_ref(), true)
}

/// A JSON file read as a `T`; None when it is missing or doesn't read as one.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    std::fs::read(path).ok().and_then(|d| serde_json::from_slice(&d).ok())
}

/// `v` as pretty JSON, written whole or not at all (`write_atomic`).
pub fn write_json(path: &std::path::Path, v: &impl Serialize) -> std::io::Result<()> {
    write_atomic(path, serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?)
}

/// `v` as pretty JSON, in a file only its owner can read (`write_private`).
pub fn write_private_json(path: &std::path::Path, v: &impl Serialize) -> std::io::Result<()> {
    write_private(path, serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?)
}

/// The files in `dir()` that hold logins, tokens or keys.
#[cfg(unix)]
pub const PRIVATE_FILES: [&str; 8] = [
    "accounts.json",
    "codex.json",
    "openrouter.json",
    "nodeclub.json",
    "freebuff.json",
    "freebuff-device.json",
    "freebuff-device-key.json",
    "local.json",
];

/// Makes the login files from before `write_private` private too.
pub fn make_private() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        for name in PRIVATE_FILES {
            let p = d.join(name);
            if std::fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

/// Where conversations are kept: `conversations` in `dir()`. Under test, a
/// folder of the test run's own instead, so the sessions tests make never
/// show up in the user's list (accounts and settings still come from
/// `dir()`, for the live tests).
pub fn conversations_dir() -> PathBuf {
    #[cfg(test)]
    {
        let d = std::env::temp_dir().join(format!("oak-test-conversations-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }
    #[cfg(not(test))]
    dir().join("conversations")
}

/// A conversation shared by its tab and its worker: a mutex that tells
/// when the conversation may have changed. Each time its guard hands out
/// `&mut Conversation` (every change goes through that), the conversation
/// gets a new revision, unique across all conversations and never reused,
/// so what was worked out from one revision (the transcript's items,
/// `chat_view`) is still right while the revision is the same.
pub struct Shared {
    conv: std::sync::Mutex<Conversation>,
    rev: std::sync::atomic::AtomicU64,
}

/// The next revision (`Shared`).
fn next_rev() -> u64 {
    static REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    REV.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Shared {
    pub fn new(conv: Conversation) -> Shared {
        Shared { conv: std::sync::Mutex::new(conv), rev: next_rev().into() }
    }

    /// Locks the conversation, as `Mutex::lock` does.
    pub fn lock(&self) -> std::sync::LockResult<SharedGuard<'_>> {
        let guard = |g| SharedGuard { g, rev: &self.rev };
        match self.conv.lock() {
            Ok(g) => Ok(guard(g)),
            Err(e) => Err(std::sync::PoisonError::new(guard(e.into_inner()))),
        }
    }
}

/// A locked conversation (`Shared::lock`).
pub struct SharedGuard<'a> {
    g: std::sync::MutexGuard<'a, Conversation>,
    rev: &'a std::sync::atomic::AtomicU64,
}

impl SharedGuard<'_> {
    /// The conversation's revision: the same until it is next changed.
    pub fn rev(&self) -> u64 {
        // Written only by the lock's holder, so the mutex orders it.
        self.rev.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl std::ops::Deref for SharedGuard<'_> {
    type Target = Conversation;
    fn deref(&self) -> &Conversation {
        &self.g
    }
}

impl std::ops::DerefMut for SharedGuard<'_> {
    fn deref_mut(&mut self) -> &mut Conversation {
        self.rev.store(next_rev(), std::sync::atomic::Ordering::Relaxed);
        &mut self.g
    }
}

/// A conversation's messages, read as a `Vec<Value>`, with each one's JSON
/// as last saved: a save serializes only the messages added or changed
/// since (`Conversation::save`). Changing a message goes through the
/// methods here, which forget the saved JSON from that message on.
///
/// Loaded from its file, a conversation's messages are kept as that JSON
/// and parsed when first read: one never opened or worked on this run
/// takes about the size of its file in memory, not two thirds more.
pub struct Messages {
    /// Unset until read, when `saved` holds every message.
    list: std::sync::OnceLock<Vec<Value>>,
    /// The JSON of `list[..saved.len()]`, one entry a message.
    saved: std::sync::Mutex<Vec<std::sync::Arc<[u8]>>>,
    /// Looked at while unread: no call was left without a result
    /// (`agent::answer_lost_calls`), and no rewrite marker is in them
    /// (`unslop::start`).
    pub(crate) no_lost_calls: bool,
    pub(crate) no_rewrites: bool,
}

/// How many conversations' messages were parsed (the frame benchmark).
#[cfg(test)]
pub(crate) static PARSED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn parse_messages(saved: &[std::sync::Arc<[u8]>]) -> Vec<Value> {
    #[cfg(test)]
    PARSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // (Each was read from JSON as one value: it reads again.)
    saved.iter().map(|m| serde_json::from_slice(m).unwrap_or_default()).collect()
}

impl Default for Messages {
    fn default() -> Messages {
        Vec::new().into()
    }
}

impl Messages {
    // Parsed, the file's JSON is let go: a conversation only looked at
    // doesn't hold both (its next save serializes it all).
    fn list(&self) -> &Vec<Value> {
        self.list.get_or_init(|| parse_messages(&std::mem::take(&mut *self.saved.lock().unwrap_or_else(|e| e.into_inner()))))
    }

    fn list_mut(&mut self) -> &mut Vec<Value> {
        if self.list.get().is_none() {
            let list = parse_messages(&std::mem::take(self.saved.get_mut().unwrap_or_else(|e| e.into_inner())));
            let _ = self.list.set(list);
        }
        self.list.get_mut().unwrap()
    }

    /// Forgets the saved JSON of message `i` and those after it.
    fn changed(&mut self, i: usize) -> &mut Vec<Value> {
        self.list_mut();
        self.saved.get_mut().unwrap_or_else(|e| e.into_inner()).truncate(i);
        self.list.get_mut().unwrap()
    }

    pub fn push(&mut self, m: Value) {
        self.list_mut().push(m);
    }

    pub fn truncate(&mut self, n: usize) {
        self.changed(n).truncate(n);
    }

    pub fn last_mut(&mut self) -> Option<&mut Value> {
        let n = self.list().len();
        self.changed(n.saturating_sub(1)).last_mut()
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.changed(i).get_mut(i)
    }

    /// Every message, to change any of them.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Value> {
        self.changed(0).iter_mut()
    }

    /// Each message's JSON as read from the file, while none has been read
    /// (parsed) yet.
    pub fn unread(&self) -> Option<Vec<std::sync::Arc<[u8]>>> {
        let saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        self.list.get().is_none().then(|| saved.clone())
    }

    /// Each message's JSON, brought up to date: only the messages without
    /// it are serialized (none, when they were never read).
    fn json(&self) -> Vec<std::sync::Arc<[u8]>> {
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = self.list.get() {
            let from = saved.len();
            let mut buf = vec![];
            for m in &list[from..] {
                buf.clear();
                serde_json::to_writer(&mut buf, m).unwrap();
                saved.push(buf.as_slice().into());
            }
        }
        saved.clone()
    }
}

impl From<Vec<Value>> for Messages {
    fn from(list: Vec<Value>) -> Messages {
        Messages { list: list.into(), saved: Default::default(), no_lost_calls: false, no_rewrites: false }
    }
}

impl std::ops::Deref for Messages {
    type Target = Vec<Value>;
    fn deref(&self) -> &Vec<Value> {
        self.list()
    }
}

impl<I: std::slice::SliceIndex<[Value]>> std::ops::Index<I> for Messages {
    type Output = I::Output;
    fn index(&self, i: I) -> &I::Output {
        &self.list()[i]
    }
}

impl std::ops::IndexMut<usize> for Messages {
    fn index_mut(&mut self, i: usize) -> &mut Value {
        &mut self.changed(i)[i]
    }
}

impl Clone for Messages {
    fn clone(&self) -> Messages {
        let saved = self.saved.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Messages { list: self.list.clone(), saved: saved.into(), no_lost_calls: self.no_lost_calls, no_rewrites: self.no_rewrites }
    }
}

impl<'a> IntoIterator for &'a Messages {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;
    fn into_iter(self) -> Self::IntoIter {
        self.list().iter()
    }
}

impl PartialEq for Messages {
    fn eq(&self, other: &Messages) -> bool {
        self.list() == other.list()
    }
}

impl PartialEq<Vec<Value>> for Messages {
    fn eq(&self, other: &Vec<Value>) -> bool {
        self.list() == other
    }
}

impl PartialEq<Messages> for [Value] {
    fn eq(&self, other: &Messages) -> bool {
        self == other.list().as_slice()
    }
}

impl std::fmt::Debug for Messages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.list().fmt(f)
    }
}

thread_local! {
    /// `Conversation::save` is serializing everything but the messages,
    /// which it adds from their saved JSON.
    static WITHOUT_MESSAGES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Serialize for Messages {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if WITHOUT_MESSAGES.get() { [(); 0].serialize(s) } else { self.list().serialize(s) }
    }
}

impl<'de> Deserialize<'de> for Messages {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Messages, D::Error> {
        let raw = Vec::<Box<serde_json::value::RawValue>>::deserialize(d)?;
        let saved = raw.iter().map(|m| m.get().as_bytes().into()).collect::<Vec<_>>();
        Ok(Messages { list: std::sync::OnceLock::new(), saved: saved.into(), no_lost_calls: false, no_rewrites: false })
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub folder: PathBuf,
    /// Who serves the conversation; `model` is one of its models. Unset
    /// (made in code), the model's (`Conversation::provider`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<crate::providers::Kind>,
    pub model: String,
    pub effort: String,
    /// The system prompt it was started with: an id in
    /// `agent::prompt::ROLES` (empty, from before roles: the default).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// The transcript, in the Oak format (`crate::oak`): only ever
    /// appended to.
    pub messages: Messages,
    /// The format `messages` is in: a file without one is from before the
    /// Oak format, in the Anthropic Messages format (`Conversation::upgrade`).
    #[serde(default = "Format::old")]
    pub format: Format,
    pub last_message_at: i64,
    pub next_task_id: u64,
    #[serde(default)]
    pub rolling: crate::rolling::State,
    /// What its requests showed about its pauses, for the cache's lifetime.
    #[serde(default)]
    pub cache: crate::cache_ttl::State,
    /// What goes before its messages, held while its cache lives, and the
    /// mid-conversation system messages that tell what changed since
    /// (Claude: `crate::prefix`).
    #[serde(default, skip_serializing_if = "is_default")]
    pub prefix: crate::prefix::State,
    /// The session's scratchpad folder under `.oak/tmp` in the working
    /// folder ("temp0", "temp1", ...); empty until it is made.
    #[serde(default)]
    pub scratch: String,
    /// The session has its real title (from the title side request, or
    /// SET_TITLE in older sessions); until then it is the first line of its
    /// first message.
    #[serde(default)]
    pub titled: bool,
    /// Set with /goal; empty when there is none.
    #[serde(default)]
    pub goal: String,
    /// How the model says it completed the goal (GOAL_JUSTIFICATION).
    #[serde(default)]
    pub goal_justification: String,
    /// The model called GOAL_FINISH: the goal ends, and control goes back
    /// to the user, when it ends its turn.
    #[serde(default)]
    pub goal_finished: bool,
    /// The user paused the goal: it stays (and the model still has it), but
    /// OAK doesn't ask the model to carry on with it when it ends its turn.
    /// Only OAK knows; nothing about it is sent.
    #[serde(default)]
    pub goal_paused: bool,
    /// While a goal is active, what the model asked to ask the user about
    /// (REQUEST_ASK), waiting for the user to accept or decline; empty
    /// when nothing waits.
    #[serde(default)]
    pub ask_request: String,
    /// The user accepted a REQUEST_ASK: the model may call ASK once.
    #[serde(default)]
    pub ask_granted: bool,
    /// The model's session memory (NOTE_NEW / NOTE_UPDATE / NOTE_DELETE).
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default)]
    pub next_note_id: u64,
    #[serde(default)]
    pub heartbeats: Vec<Heartbeat>,
    #[serde(default)]
    pub next_heartbeat_id: u64,
    /// The progress meters the model set up (PROGRESS_SET), shown under
    /// the goal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meters: Vec<Meter>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub next_meter_id: u64,
    /// Lines in the transcript that are for the user only, never sent.
    #[serde(default)]
    pub marks: Vec<Mark>,
    /// The goal, notes and heartbeats as last put in the system prompt.
    #[serde(default)]
    pub memory: Option<Memory>,
    /// Stopped by a usage limit: when it resets (ms since 1970).
    #[serde(default)]
    pub limit_wait: Option<i64>,
    /// Dollars this session spent through OpenRouter: its replies and
    /// context summaries, as OpenRouter priced them.
    #[serde(default)]
    pub spent: f64,
    /// Tokens this session's replies used, as the providers reported them
    /// (any provider: on a subscription this is the only measure of cost).
    #[serde(default)]
    pub tokens: Tokens,
    /// What the agents this session deleted used (their `tokens` and
    /// `spent`), kept when their conversations are removed.
    #[serde(default)]
    pub agent_tokens: Tokens,
    /// Through OpenRouter: the provider that served the last reply.
    #[serde(default)]
    pub served_by: String,
    /// Paused by the user: nothing is sent until resumed.
    #[serde(default)]
    pub paused: bool,
    /// For a subagent: the conversation that started it (AGENT_NEW), and
    /// its number there. None for a session the user started.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub agent_id: u64,
    /// A retained fork shown in a side-chat window, not a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_chat_of: Option<String>,
    /// A subagent the main session let start agents of its own (AGENT_NEW
    /// `can_spawn`): a sub-orchestrator. One level only: its agents can't.
    #[serde(default)]
    pub spawner: bool,
    /// The subagents this session started, by number.
    #[serde(default)]
    pub agents: Vec<AgentRef>,
    #[serde(default)]
    pub next_agent_id: u64,
    /// Messages from other sessions (subagents, or for a subagent the main
    /// session and other agents), waiting for the next request.
    #[serde(default)]
    pub inbox: Vec<String>,
    /// Notes for the model that wait for the next request without causing
    /// one: that the goal was cleared.
    #[serde(default)]
    pub held: Vec<String>,
    /// The conversation was deleted (AGENT_DELETE): saving it does nothing,
    /// so its file isn't written again by a worker still winding down.
    #[serde(skip)]
    pub deleted: bool,
}

/// Token totals over many requests: see `Conversation::tokens`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub requests: u64,
    /// Prompt tokens neither read from nor written to a cache.
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub output: u64,
    /// Dollars through OpenRouter (only in `agent_tokens`, where the
    /// agents' `spent` goes).
    #[serde(default)]
    pub usd: f64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.requests += o.requests;
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.usd += o.usd;
    }

    /// "212 requests, 31.2M read from cache, 0.4M written, 0.1M fresh, 170k out".
    pub fn describe(&self) -> String {
        let m = |n: u64| if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) } else { format!("{}k", (n + 500) / 1000) };
        let mut s = format!("{} requests, {} read from cache, {} written, {} fresh input, {} output",
            self.requests, m(self.cache_read), m(self.cache_write), m(self.input), m(self.output));
        if self.usd > 0.0 {
            s += &format!(", ${:.2} through OpenRouter", self.usd);
        }
        s
    }
}

/// A transcript format (`crate::oak::FORMAT`); a new conversation's is the
/// current one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Format(pub u32);

impl Format {
    /// The Anthropic Messages format, which transcripts were kept in before.
    fn old() -> Format {
        Format(0)
    }
}

impl Default for Format {
    fn default() -> Format {
        Format(crate::oak::FORMAT)
    }
}

impl Conversation {
    /// Brings a transcript kept in an older format to the current one;
    /// true when it changed. Thinking that doesn't say which model wrote it
    /// counts as the conversation's model's provider's.
    pub fn upgrade(&mut self) -> bool {
        if self.format.0 >= crate::oak::FORMAT {
            return false;
        }
        let provider = crate::providers::Kind::of(&self.model);
        if self.format.0 == 0 {
            self.messages = crate::oak::from_anthropic(&self.messages, provider.id()).into();
        }
        self.provider = Some(provider);
        self.format = Format::default();
        true
    }

    /// Who serves the conversation.
    pub fn provider(&self) -> crate::providers::Kind {
        self.provider.unwrap_or_else(|| crate::providers::Kind::of(&self.model))
    }
}

/// Where conversations are copied, as they were, before their first
/// upgrade to the Oak format.
pub fn backup_dir() -> PathBuf {
    let d = conversations_dir();
    let name = d.file_name().unwrap_or_default().to_string_lossy().into_owned();
    d.with_file_name(format!("{name}-backup-anthropic"))
}

/// A subagent a session started: its number there, its conversation and
/// the name the model gave it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRef {
    pub id: u64,
    pub conv: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: u64,
    pub text: String,
    /// Rewrite notes: the note rewritten for the notes box (`crate::unslop`),
    /// dropped when it changes. Never sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<Rewrite>,
}

/// A note's rewrite: under way, done (`text`) or failed (`error`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rewrite {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A message the model asked to get every `every_min` minutes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub id: u64,
    pub message: String,
    pub every_min: u64,
    /// When it is next due (ms since 1970).
    pub next_at: i64,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A progress meter: OAK runs `command` every `every_min` minutes (and
/// when it is set up) and shows the value it prints (`crate::progress`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    pub id: u64,
    pub label: String,
    pub command: String,
    pub every_min: u64,
    /// The last good reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<crate::progress::Reading>,
    /// Why the last run gave no value; empty when it gave one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// When the last run ended, and when the next is due (ms since 1970).
    #[serde(default)]
    pub ran_at: i64,
    #[serde(default)]
    pub next_at: i64,
    /// Bumped when PROGRESS_SET gives it a new command: a run of the old
    /// one is killed and its result dropped.
    #[serde(skip)]
    pub generation: u64,
    /// A watcher thread runs it (`progress::watch`).
    #[serde(skip)]
    pub watched: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkKind {
    Goal,
    Effort,
    /// A usage limit stopped the conversation; `time` is when it resets.
    Limit,
    /// The user stopped a heartbeat, or removed a progress meter.
    Heartbeat,
    /// The account hit its usage limit and requests went to another;
    /// `text` names it.
    Rotated,
    /// The model ended its turn without finishing the goal, so OAK asked
    /// it to carry on.
    GoalContinued,
    /// The model finished the goal; `text` is its justification.
    GoalFinished,
    /// Instruction files were loaded; `text` lists them.
    Instructions,
    /// Files the model had seen changed on disk, not by OAK; `text` lists them.
    FilesChanged,
    /// Smart context summarized the conversation: put where the
    /// transcript stood when the summary was made. `text` is the summary
    /// and `done` the cut (the model sees the summary in place of the
    /// messages before it).
    Summarized,
    /// At the start of a session (Claude models): the billing header and
    /// the Claude Code identity sent before the system prompt.
    Identity,
    /// At the start of a session: the tool definitions sent with every
    /// request; `text` says how many.
    Tools,
    /// The user switched the conversation to another model; `text` is its id.
    Model,
    /// A Freebuff session hour ran out mid-turn; the grace period began.
    FreebuffHour,
    /// The Freebuff grace period ran out; the model asks to carry on.
    FreebuffGrace,
    /// A turn failed or was stopped, where it happened (an Oak agent's tab);
    /// `text` says why.
    Failed,
}

/// A mark's `sent` in parts, each shown in a box of its own under its
/// title (say, the system prompt and the start of the first message).
const PART: char = '\u{1e}';
const TITLE: char = '\u{1f}';

pub fn sent_parts(parts: &[(&str, &str)]) -> String {
    parts.iter().map(|(title, body)| format!("{PART}{title}{TITLE}{body}")).collect()
}

/// The parts of a mark's `sent` (one untitled part for plain text).
pub fn split_sent(sent: &str) -> Vec<(&str, &str)> {
    if !sent.starts_with(PART) {
        return vec![("", sent)];
    }
    sent.split(PART).skip(1).map(|p| p.split_once(TITLE).unwrap_or(("", p))).collect()
}

/// A transcript line shown before message `at`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub at: usize,
    pub kind: MarkKind,
    pub text: String,
    #[serde(default)]
    pub time: i64,
    /// For a limit: when OAK continued or stopped waiting (0 while waiting).
    /// Its `text` is then "continued" or "stopped" (empty while waiting).
    #[serde(default)]
    pub done: i64,
    /// What the model was sent along with the line, word for word (the
    /// transcript shows it when the line is opened, as the "model view");
    /// empty for lines the model isn't told about. See `sent_parts`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sent: String,
}

impl Mark {
    /// A line at `at`, not done, sent with nothing.
    pub fn new(at: usize, kind: MarkKind, text: impl Into<String>, time: i64) -> Self {
        Mark { at, kind, text: text.into(), time, done: 0, sent: String::new() }
    }
}

/// The session memory block in the system prompt. It changes only when the
/// prompt cache is being rebuilt anyway (a summary was applied, or the cache
/// went cold), so notes don't cost a rewrite of the whole context.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Memory {
    pub text: String,
    /// The instruction files (CLAUDE.md or AGENTS.md) and the project
    /// memory index, for the start of the conversation.
    #[serde(default)]
    pub context: String,
    /// The instruction files it holds.
    #[serde(default)]
    pub files: Vec<String>,
    /// The summary cut it was made at.
    pub base: usize,
    /// When a request last used it.
    pub used_at: i64,
}

impl Conversation {
    /// The transcript's file name: the working folder's name, the title (if
    /// there is one, shortened) and the id, which keeps names unique, e.g.
    /// `oak - Fix the parser - 1767000000000.json`.
    pub fn file_name(&self) -> String {
        let folder = self.folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let title: String = self.title.head(50);
        let parts: Vec<String> = [folder, title].iter().map(|p| clean_name(p)).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() { format!("{}.json", self.id) } else { format!("{} - {}.json", parts.join(" - "), self.id) }
    }

    /// Saves the conversation. When its name changed (it got a title), the
    /// file under the old name is removed once the new one is written.
    ///
    /// Serialized here, written to disk by the writer thread (in order), so
    /// a long conversation's lock isn't held for the write. `flush` waits.
    #[track_caller]
    pub fn save(&self) {
        if self.deleted {
            return;
        }
        let started = std::time::Instant::now();
        let from = std::panic::Location::caller();
        let data = self.data();
        let (lock, wake) = &*QUEUE;
        lock.lock().unwrap().writes.push(Write { id: self.id.clone(), name: self.file_name(), data, started, from });
        wake.notify_all();
        static WRITER: std::sync::Once = std::sync::Once::new();
        WRITER.call_once(|| drop(std::thread::Builder::new().name("conversation writer".into()).spawn(write_conversations)));
    }
}

impl Conversation {
    /// What its file holds: `serde_json::to_vec(self)`, with only the
    /// messages changed since the last save serialized.
    fn data(&self) -> Data {
        // Everything but the messages, with `"messages":[]` where they go.
        // (Its first match is the field: inside a JSON string a quote is
        // always escaped.)
        WITHOUT_MESSAGES.set(true);
        let rest = serde_json::to_vec(self);
        WITHOUT_MESSAGES.set(false);
        let mut rest = rest.unwrap();
        const FIELD: &[u8] = b"\"messages\":[";
        let at = rest.windows(FIELD.len()).position(|w| w == FIELD).expect("a messages field") + FIELD.len();
        let after = rest.split_off(at);
        Data { before: rest, messages: self.messages.json(), after }
    }
}

/// A conversation's file: its messages' JSON, comma separated, between
/// `before` and `after`.
struct Data {
    before: Vec<u8>,
    messages: Vec<std::sync::Arc<[u8]>>,
    after: Vec<u8>,
}

impl Data {
    fn len(&self) -> usize {
        self.before.len() + self.messages.iter().map(|m| m.len() + 1).sum::<usize>().saturating_sub(1) + self.after.len()
    }

    fn write_to(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        out.write_all(&self.before)?;
        for (i, m) in self.messages.iter().enumerate() {
            if i > 0 {
                out.write_all(b",")?;
            }
            out.write_all(m)?;
        }
        out.write_all(&self.after)
    }

    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        let mut v = vec![];
        self.write_to(&mut v).unwrap();
        v
    }
}

/// A conversation's file, waiting to be written.
struct Write {
    id: String,
    name: String,
    data: Data,
    started: std::time::Instant,
    /// Where it was saved from (for the benchmark timeline).
    from: &'static std::panic::Location<'static>,
}

#[derive(Default)]
struct Queue {
    writes: Vec<Write>,
    /// The writer is writing what it took from `writes`.
    busy: bool,
}

/// The saves not yet written, and the writer's (and `flush`'s) wake-up.
static QUEUE: std::sync::LazyLock<(std::sync::Mutex<Queue>, std::sync::Condvar)> = std::sync::LazyLock::new(Default::default);

/// The writer thread: writes the saves in the order they were made, only
/// the newest of a conversation's when several wait.
fn write_conversations() {
    let (lock, wake) = &*QUEUE;
    loop {
        let writes = {
            let mut q = wake.wait_while(lock.lock().unwrap(), |q| q.writes.is_empty()).unwrap();
            q.busy = true;
            std::mem::take(&mut q.writes)
        };
        for (i, w) in writes.iter().enumerate() {
            if !writes[i + 1..].iter().any(|later| later.id == w.id) {
                write_conversation(w);
            } else {
                crate::bench::log(&w.id, "save_superseded", serde_json::json!({"from": format!("{}:{}", w.from.file(), w.from.line())}));
            }
        }
        lock.lock().unwrap().busy = false;
        wake.notify_all();
    }
}

fn write_conversation(w: &Write) {
    let conversations = conversations_dir();
    let path = conversations.join(&w.name);
    let tmp = conversations.join(format!("{}.tmp", w.id));
    let written = std::fs::File::create(&tmp).and_then(|f| {
        let mut out = std::io::BufWriter::with_capacity(1 << 16, f);
        w.data.write_to(&mut out)?;
        std::io::Write::flush(&mut out)
    });
    if written.is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        let old = FILES.lock().unwrap().insert(w.id.clone(), w.name.clone());
        let old = old.unwrap_or_else(|| format!("{}.json", w.id));
        if old != w.name {
            let _ = std::fs::remove_file(conversations.join(old));
        }
    }
    crate::bench::log(&w.id, "save", serde_json::json!({"ms": w.started.elapsed().as_secs_f64() * 1000.0, "bytes": w.data.len(), "from": format!("{}:{}", w.from.file(), w.from.line())}));
}

/// Waits until every save made so far is on disk.
pub fn flush() {
    let (lock, wake) = &*QUEUE;
    drop(wake.wait_while(lock.lock().unwrap(), |q| !q.writes.is_empty() || q.busy).unwrap());
}

/// Each conversation's file name as last loaded or saved, by id.
static FILES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

/// Text fit for a Windows file name: characters it can't hold become
/// spaces, runs of spaces one, no leading or trailing dots or spaces.
fn clean_name(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c }).collect();
    one_line(&s).trim_matches(['.', ' ']).to_string()
}

/// Deletes a conversation's file (AGENT_DELETE, and tests).
pub fn remove_conversation(id: &str) {
    // (A save still waiting would write it again.)
    flush();
    let conversations = conversations_dir();
    let name = FILES.lock().unwrap().remove(id);
    // Closing a Freebuff conversation gives its hourly session back.
    let freebuff = name
        .as_ref()
        .and_then(|name| std::fs::read_to_string(conversations.join(name)).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v["model"].as_str().map(crate::freebuff::serves))
        .unwrap_or(false);
    if freebuff {
        crate::freebuff::release();
    }
    if let Some(name) = name {
        let _ = std::fs::remove_file(conversations.join(name));
    }
    let _ = std::fs::remove_file(conversations.join(format!("{id}.json")));
}

/// Reads and parses conversation files on every core (most of Oak's
/// startup is this), the biggest first; in `files`' order.
fn read_all(files: &[std::fs::DirEntry], look: &(dyn Fn(&mut Conversation) + Sync)) -> Vec<Option<Conversation>> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(files[i].metadata().map_or(0, |m| m.len())));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(files.len()).max(1);
    let mut parsed: Vec<(usize, Option<Conversation>)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = vec![];
                    while let Some(&i) = order.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        let mut c = read_json::<Conversation>(&files[i].path());
                        c.iter_mut().for_each(look);
                        out.push((i, c));
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    });
    parsed.sort_by_key(|p| p.0);
    parsed.into_iter().map(|p| p.1).collect()
}

/// `load_conversations_then`, looking at nothing.
#[cfg(test)]
pub fn load_conversations() -> Vec<Conversation> {
    load_conversations_then(&|_| {})
}

/// Every saved conversation, with `look` done to each as soon as it is
/// read, on the thread that read it (while its file's JSON is at hand). A
/// conversation found under two names (OAK stopped between writing the new
/// file and removing the old one) is loaded from the one saved last.
pub fn load_conversations_then(look: &(dyn Fn(&mut Conversation) + Sync)) -> Vec<Conversation> {
    flush();
    let mut found: std::collections::HashMap<String, (std::time::SystemTime, String, Conversation)> = Default::default();
    let mut stale = vec![];
    if let Ok(entries) = std::fs::read_dir(conversations_dir()) {
        let files: Vec<_> = entries.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).collect();
        for (e, c) in files.iter().zip(read_all(&files, look)) {
            let Some(c) = c else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata();
            let at = meta.as_ref().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            match found.get(&c.id) {
                Some((t, _, _)) if *t >= at => stale.push(name),
                earlier => {
                    stale.extend(earlier.map(|e| e.1.clone()));
                    found.insert(c.id.clone(), (at, name, c));
                }
            }
        }
    }
    let conversations = conversations_dir();
    for name in stale {
        let _ = std::fs::remove_file(conversations.join(name));
    }
    let mut out = vec![];
    for (id, (_, name, mut c)) in found {
        // A transcript in an older format is upgraded, a copy of the file as
        // it was kept first (once: an earlier copy is the original). Without
        // its copy, it is written only when it changes anyway.
        let upgraded = c.upgrade() && {
            let backup = backup_dir().join(&name);
            backup.exists() || std::fs::create_dir_all(backup_dir()).and_then(|_| std::fs::copy(conversations.join(&name), &backup)).is_ok()
        };
        // Files from before transcripts were named after their sessions
        // get their name now.
        let renamed = name != c.file_name();
        FILES.lock().unwrap().insert(id, name);
        if renamed || upgraded {
            c.save();
        }
        out.push(c);
    }
    // The files renamed above are on disk by the time this returns.
    flush();
    out
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The theme's id: "oak" (jet black, white, oak green), or a plugin's
    /// (`oak_sdk::ThemeDef`; Oak's shows while that plugin isn't loaded).
    /// Empty only as read from a file from before themes had ids
    /// (`load_settings` then takes it from `oak`).
    #[serde(default = "String::new")]
    pub theme: String,
    /// From before themes had ids: the Oak theme instead of Claude's.
    #[serde(default, skip_serializing)]
    oak: Option<bool>,
    /// The interface font's id: "plex" (IBM Plex Sans), or a plugin's
    /// (`oak_sdk::FontDef`).
    pub font: String,
    pub folder: String,
    /// Who serves new conversations (unset: the model's, from before this
    /// was kept); `model` is one of its models.
    pub provider: Option<crate::providers::Kind>,
    pub model: String,
    pub effort: String,
    /// The system prompt for new conversations (an id in
    /// `agent::prompt::ROLES`; empty or unknown: the default).
    pub role: String,
    /// Smart context for new conversations.
    pub rolling: bool,
    /// Summarize past this many tokens of context...
    pub roll_trigger: u64,
    /// ...keeping about this many of the newest verbatim.
    pub roll_keep: u64,
    /// Who writes a local model's summaries: "local" (the model itself,
    /// thinking off), "deepseek" (through OpenRouter) or "haiku" (a Claude
    /// account); see `rolling::Summarizer`.
    pub local_summarizer: String,
    /// The prompt cache's lifetime: "auto" (per conversation, see
    /// `cache_ttl`), "5m" or "1h".
    pub cache: String,
    /// Server-side threads: a request continues the server's copy of the
    /// conversation instead of sending all of it again.
    pub threads: bool,
    /// Keep-alive: while a conversation is idle, its five-minute prompt
    /// cache is read again before it expires (for up to half an hour), so
    /// a short pause doesn't write the whole context again.
    pub keep_alive: bool,
    /// Cache diagnostics: the API says why a request missed the cache, in
    /// the usage log.
    pub cache_diagnostics: bool,
    /// Stable prefix (Claude): the date and the AGENT tool's model list
    /// change in mid-conversation system messages, and the goal's tools come
    /// and go as inline tool changes, instead of rewriting the system prompt
    /// and tools (and so the whole cache).
    pub stable_prefix: bool,
    /// Account rotation: a used-up account's requests go to another.
    pub rotation: bool,
    /// The whole interface's zoom (egui's zoom factor, 1 = 100%), changed
    /// with Ctrl+Plus, Ctrl+Minus and Ctrl+0 or in Settings.
    pub zoom: f32,
    /// The image viewer zooms a set step per click (and back out) instead
    /// of with the wheel.
    pub click_zoom: bool,
    /// Linux: how many times further a touchpad scrolls than the pixels it
    /// reports (Wayland passes them on 1:1 with the fingers, which is slow).
    pub touchpad_speed: f32,
    /// The conversations open as tabs, in order.
    pub tabs: Vec<String>,
    /// Unslop: the transcript shows the model's messages rewritten
    /// (`crate::unslop`); what the model is sent is unchanged.
    pub unslop: bool,
    /// With Unslop, the transcript leaves out the model's drafts: only the
    /// rewrites show.
    pub unslop_no_drafts: bool,
    /// With Unslop, thinking is rewritten too, as it streams in.
    pub unslop_thinking: bool,
    /// With Unslop, the notes box shows the notes rewritten.
    pub unslop_notes: bool,
    /// The model is told to tidy up its notes once they pass
    /// `agent::NOTES_ALERT_TOKENS`.
    pub notes_alert: bool,
    /// The OpenRouter model that names sessions (an id in
    /// `crate::openrouter::TITLE_CHOICES`).
    pub title_model: String,
    /// How OpenRouter requests pick a provider (an id of
    /// `crate::openrouter::Routing`).
    pub or_routing: String,
    /// The conversations paused (or ultrapaused) by closing Oak while they
    /// worked: the next start offers to resume them, once.
    pub paused_on_close: Vec<String>,
    /// The Duck.ai page's model (a duck.ai model id) and whether it reasons
    /// ("Reasoning" rather than "Fast").
    pub duck_model: String,
    pub duck_reasoning: bool,
    /// Scratchpads idle this long (seconds) are emptied (`crate::scratch`);
    /// 0: never.
    pub scratch_clear: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "oak".into(),
            oak: None,
            font: "plex".into(),
            folder: String::new(),
            provider: None,
            model: String::new(),
            effort: "high".into(),
            role: String::new(),
            rolling: true,
            roll_trigger: crate::rolling::DEFAULT.trigger,
            roll_keep: crate::rolling::DEFAULT.keep,
            local_summarizer: "local".into(),
            cache: "auto".into(),
            threads: false,
            keep_alive: true,
            cache_diagnostics: true,
            stable_prefix: true,
            rotation: true,
            zoom: 1.0,
            click_zoom: false,
            touchpad_speed: 3.0,
            tabs: vec![],
            unslop: false,
            unslop_no_drafts: false,
            unslop_thinking: false,
            unslop_notes: false,
            notes_alert: true,
            title_model: crate::openrouter::TITLE_CHOICES[0].0.into(),
            or_routing: "balanced".into(),
            paused_on_close: vec![],
            duck_model: crate::duck::DEFAULT_MODEL.into(),
            duck_reasoning: false,
            scratch_clear: 0,
        }
    }
}

pub fn load_settings() -> Settings {
    let path = dir().join("settings.json");
    let Ok(data) = std::fs::read(&path) else { return Settings::default() };
    let mut s: Settings = serde_json::from_slice(&data).unwrap_or_else(|_| {
        // Keep what was unreadable, instead of the next save overwriting it.
        let _ = std::fs::rename(&path, dir().join("settings.json.unreadable"));
        Settings::default()
    });
    s.name_theme();
    s
}

impl Settings {
    /// A file from before themes had ids says only whether the theme is
    /// Oak's: the other was Claude's (which a plugin brings now).
    fn name_theme(&mut self) {
        if self.theme.is_empty() {
            self.theme = if self.oak == Some(false) { "claude" } else { "oak" }.into();
        }
        self.oak = None;
    }
}

pub fn save_settings(s: &Settings) {
    let _ = write_json(&dir().join("settings.json"), s);
}

#[cfg(test)]
mod tests {
    /// Settings from before themes had ids keep their theme; new ones
    /// start with Oak's.
    #[test]
    fn old_settings_keep_their_theme() {
        let read = |text: &str| {
            let mut s: super::Settings = serde_json::from_str(text).unwrap();
            s.name_theme();
            s.theme
        };
        assert_eq!(read(r#"{"oak": true, "font": "plex"}"#), "oak");
        assert_eq!(read(r#"{"oak": false, "font": "anthropic"}"#), "claude");
        assert_eq!(read(r#"{"theme": "claude", "oak": true}"#), "claude");
        assert_eq!(read("{}"), "oak");
        assert_eq!(super::Settings::default().theme, "oak");
        let saved = serde_json::to_value(super::Settings::default()).unwrap();
        assert!(saved.get("oak").is_none() && saved["theme"] == "oak");
    }

    /// A save writes what serializing the whole conversation would, however
    /// its messages changed since the last one (only those are redone).
    #[test]
    fn save_writes_the_whole_conversation() {
        use serde_json::json;
        let mut c = super::Conversation {
            id: "1".into(),
            title: "\"messages\":[] in a title".into(),
            messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "\"messages\":[1]"}]})].into(),
            ..Default::default()
        };
        let same = |c: &super::Conversation| assert_eq!(String::from_utf8(c.data().bytes()).unwrap(), serde_json::to_string(c).unwrap());
        same(&c);
        c.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": "two"}]}));
        c.messages.push(json!({"role": "user", "content": [{"type": "text", "text": "three"}]}));
        same(&c);
        c.messages.last_mut().unwrap()["content"][0]["text"] = json!("three, changed");
        same(&c);
        c.messages[0]["content"][0]["text"] = json!("one, changed");
        same(&c);
        if let Some(m) = c.messages.get_mut(1) {
            m["content"][0]["text"] = json!("two, changed");
        }
        same(&c);
        c.messages.truncate(2);
        c.notes.push(super::Note { id: 1, text: "a note".into(), rewrite: None });
        same(&c);
        c.messages.iter_mut().for_each(|m| m["seen"] = json!(true));
        same(&c);
        let copy = c.clone();
        c.messages.push(json!({"role": "assistant", "content": []}));
        same(&copy);
        same(&c);
        c.messages = Vec::new().into();
        same(&c);
        let loaded: super::Conversation = serde_json::from_slice(&copy.data().bytes()).unwrap();
        assert_eq!(loaded.messages, copy.messages);
        same(&loaded);
    }

    /// A deleted agent's tokens add up in its parent, and old transcripts
    /// without the fields still load.
    #[test]
    fn token_totals_add_up_and_default() {
        let mut a = super::Tokens { requests: 2, input: 10, cache_read: 2_500_000, cache_write: 40_000, output: 1_200, usd: 0.0 };
        a.add(&super::Tokens { requests: 1, output: 800, usd: 0.25, ..Default::default() });
        assert_eq!((a.requests, a.output), (3, 2_000));
        assert_eq!(a.describe(), "3 requests, 2.5M read from cache, 40k written, 0k fresh input, 2k output, $0.25 through OpenRouter");
        let mut v = serde_json::to_value(super::Conversation::default()).unwrap();
        v.as_object_mut().unwrap().retain(|k, _| k != "tokens" && k != "agent_tokens");
        let c: super::Conversation = serde_json::from_value(v).unwrap();
        assert_eq!(c.tokens, super::Tokens::default());
    }

    #[test]
    fn atomic_writes_replace_whole_and_leave_nothing_behind() {
        let d = std::env::temp_dir().join(format!("oak-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("settings.json");
        write_atomic(&p, b"old").unwrap();
        write_atomic(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        write_private(&p, b"secret").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"secret");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    use super::*;

    /// Transcript files are named after the folder and the title, with the
    /// id last; a title arriving renames the file (the old one goes).
    #[test]
    fn files_named_after_folder_and_title() {
        let mut c = Conversation { id: format!("unit-test-name-{}", crate::api::now_ms()), folder: PathBuf::from(if cfg!(windows) { r"C:\work\oak" } else { "/work/oak" }), ..Default::default() };
        assert_eq!(c.file_name(), format!("oak - {}.json", c.id));
        c.title = "Fix: the \"parser\" / lexer?".into();
        assert_eq!(c.file_name(), format!("oak - Fix the parser lexer - {}.json", c.id));
        c.folder = PathBuf::new();
        c.title = " ... ".into();
        assert_eq!(c.file_name(), format!("{}.json", c.id));

        c.folder = PathBuf::from(if cfg!(windows) { r"C:\work\oak" } else { "/work/oak" });
        c.title.clear();
        c.save();
        flush();
        let first = conversations_dir().join(c.file_name());
        assert!(first.is_file());
        c.title = "Named".into();
        c.save();
        flush();
        assert!(!first.exists(), "the old name is gone");
        assert!(conversations_dir().join(c.file_name()).is_file());
        remove_conversation(&c.id);
        assert!(!conversations_dir().join(c.file_name()).exists());
    }

    /// Of two files with one id, the one saved last is kept, on disk too,
    /// whichever of them the folder lists first.
    #[test]
    fn two_names_keep_the_newer_file() {
        for newer_first in [false, true] {
            let id = format!("unit-test-twice-{}-{newer_first}", crate::api::now_ms());
            let old = Conversation { id: id.clone(), title: "Old".into(), ..Default::default() };
            let new = Conversation { id: id.clone(), title: "New".into(), ..Default::default() };
            let (a, b) = (conversations_dir().join(format!("a - {id}.json")), conversations_dir().join(format!("b - {id}.json")));
            let (newer, older) = if newer_first { (&a, &b) } else { (&b, &a) };
            std::fs::write(older, serde_json::to_vec(&old).unwrap()).unwrap();
            std::fs::write(newer, serde_json::to_vec(&new).unwrap()).unwrap();
            let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
            std::fs::File::options().write(true).open(older).unwrap().set_modified(past).unwrap();
            let loaded = load_conversations().into_iter().find(|c| c.id == id).unwrap();
            assert_eq!(loaded.title, "New");
            assert!(!older.exists(), "the older file goes");
            let kept = conversations_dir().join(loaded.file_name());
            let on_disk: Conversation = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
            assert_eq!(on_disk.title, "New", "the newer content stays on disk");
            remove_conversation(&id);
        }
    }

    /// Upgrades a copy of real conversations to the Oak format (run with
    /// OAK_UPGRADE_FROM set to a folder of them): each is backed up as it
    /// was, converted whole, and makes a request every provider can take.
    #[test]
    #[ignore]
    fn upgrade_real_conversations() {
        use super::*;
        let from = std::env::var_os("OAK_UPGRADE_FROM").expect("set OAK_UPGRADE_FROM to a folder of conversations");
        for e in std::fs::read_dir(from).unwrap().flatten() {
            std::fs::copy(e.path(), conversations_dir().join(e.file_name())).unwrap();
        }
        let before: std::collections::HashMap<String, usize> = std::fs::read_dir(conversations_dir())
            .unwrap()
            .flatten()
            .filter_map(|e| read_json::<Value>(&e.path()))
            .map(|v| (v["id"].as_str().unwrap_or("").to_string(), v["messages"].as_array().map_or(0, Vec::len)))
            .collect();
        let t = std::time::Instant::now();
        let first = load_conversations();
        flush();
        eprintln!("loaded and upgraded {} in {:?}", first.len(), t.elapsed());
        let backups = std::fs::read_dir(backup_dir()).map_or(0, |d| d.count());
        eprintln!("{backups} backups");
        let again = load_conversations();
        assert_eq!(first.len(), again.len());
        let old = ["thinking", "redacted_thinking", "tool_use"];
        let mut blocks = 0;
        for c in &again {
            assert_eq!(c.format, Format::default(), "{}", c.id);
            assert_eq!(before.get(&c.id), Some(&c.messages.len()), "{}", c.id);
            for m in &c.messages {
                for b in m["content"].as_array().unwrap() {
                    blocks += 1;
                    let ty = b["type"].as_str().unwrap_or("");
                    assert!(!old.contains(&ty), "{} {b}", c.id);
                    assert!(b.get("source").is_none() && b.get("tool_use_id").is_none() && b.get("thinking").is_none() && b.get("signature").is_none(), "{} {b}", c.id);
                    if ty == "tool_result" {
                        assert!(b["call_id"].is_string(), "{} {b}", c.id);
                    }
                    if ty == "reasoning" {
                        assert!(b["by"].is_string(), "{} {b}", c.id);
                    }
                }
            }
            let req = crate::oak::Request { model: c.model.clone(), messages: crate::oak::clean(c.messages.to_vec()), ..Default::default() };
            let body = crate::api::claude_body(&req);
            assert_eq!(body["messages"].as_array().unwrap().len(), c.messages.len().max(body["messages"].as_array().unwrap().len()));
            let _ = crate::codex::request_body(&req);
            let _ = crate::freebuff::chat_messages(&req.messages);
        }
        eprintln!("{blocks} blocks checked");
    }
}
