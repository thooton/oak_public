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
    write_replacing_as(path, data, private, true)
}

fn write_replacing_as(path: &std::path::Path, data: &[u8], private: bool, sync: bool) -> std::io::Result<()> {
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
        if sync { f.sync_all() } else { Ok(()) }
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

/// `write_atomic` without waiting for the data to reach the disk: for files
/// named after what they hold, checked before they are used again
/// (`blobs`), where a thousand waits would be most of the time taken.
pub fn write_atomic_unsynced(path: &std::path::Path, data: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_replacing_as(path, data.as_ref(), false, false)
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
pub const PRIVATE_FILES: [&str; 10] = [
    "accounts.json",
    "codex.json",
    "openrouter.json",
    "nodeclub.json",
    "refuseless.json",
    "musecode.json",
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

impl Shared {
    /// Locks the conversation if no one holds it.
    pub fn try_lock(&self) -> Option<SharedGuard<'_>> {
        self.conv.try_lock().ok().map(|g| SharedGuard { g, rev: &self.rev })
    }
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

/// Where a conversation's messages are on disk: its file and each
/// message's bytes in it (`start..end`), as the writer last wrote it (or
/// as it was loaded). Shared with the writer, which updates it as it
/// renames a new file into place; readers hold it while they read.
#[derive(Default)]
pub(crate) struct Disk {
    path: Option<PathBuf>,
    ranges: Vec<(u64, u64)>,
    /// The last save (`Messages::queued`) that is on disk.
    written: u64,
    /// Another copy of the conversation wrote its file since: these ranges
    /// are no longer what it holds.
    lost: bool,
}

impl Disk {
    /// The bytes of messages `from..to` as the file has them (with what
    /// separates them), and where they start.
    fn read(&self, from: usize, to: usize) -> std::io::Result<(Vec<u8>, u64)> {
        use std::io::{Read, Seek};
        let bad = || std::io::Error::other("no such messages on disk");
        if self.lost || from >= to || to > self.ranges.len() {
            return Err(bad());
        }
        let (start, end) = (self.ranges[from].0, self.ranges[to - 1].1);
        let mut f = std::fs::File::open(self.path.as_ref().ok_or_else(bad)?)?;
        f.seek(std::io::SeekFrom::Start(start))?;
        let mut buf = vec![0; (end - start) as usize];
        f.read_exact(&mut buf)?;
        Ok((buf, start))
    }

    /// Messages `from..to`, parsed (`Null` for any that can't be read).
    fn messages(&self, from: usize, to: usize) -> Vec<Value> {
        if from >= to {
            return vec![];
        }
        #[cfg(test)]
        PARSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.read(from, to) {
            Ok((buf, start)) => self.ranges[from..to].iter().map(|&(a, b)| serde_json::from_slice(&buf[(a - start) as usize..(b - start) as usize]).unwrap_or_default()).collect(),
            Err(e) => {
                crate::startup::note(format!("Couldn't read messages {from}..{to} of {:?}: {e}", self.path));
                vec![Value::Null; to - from]
            }
        }
    }
}

/// How many messages, at least, are read when a conversation is opened
/// (and each time earlier ones are needed); then more, while they come to
/// less than `WINDOW_BYTES` and `WINDOW_MAX` messages.
pub const WINDOW_MIN: usize = 60;
const WINDOW_BYTES: u64 = 1_500_000;
const WINDOW_MAX: usize = 400;

/// Where a window of messages ending at `end` starts, by their sizes.
fn window_start(ranges: &[(u64, u64)], end: usize) -> usize {
    let mut start = end;
    let mut bytes = 0;
    while start > 0 {
        let n = end - start;
        bytes += ranges[start - 1].1 - ranges[start - 1].0;
        if n >= WINDOW_MIN && (bytes > WINDOW_BYTES || n >= WINDOW_MAX) {
            break;
        }
        start -= 1;
    }
    start
}

/// A user message of tool results only: it reads with the call before it.
fn results_only(m: &Value) -> bool {
    m["role"] == "user" && m["content"].as_array().is_some_and(|c| !c.is_empty() && c.iter().all(|b| b["type"] == "tool_result"))
}

/// The messages held: `list` has every message, those before `base` as
/// `Null` (on disk only).
struct Window {
    base: usize,
    list: Vec<Value>,
}

/// A conversation's messages. Only its file's metadata is read at start:
/// the messages are read from it when first looked at, and then only the
/// last ones (a window: `WINDOW_MIN`); earlier ones as they are needed
/// (`load_from`, `load_earlier`: scrolling up, a request that sends them).
/// Through `Deref` they read as a `Vec<Value>` of every message, with
/// `Null` in place of those not read. Changing a message goes through the
/// methods here, which keep track of what is as last saved: a save writes
/// only the messages changed since, and copies the rest from the file
/// (`Conversation::save`). Messages before `base` that are on disk as they
/// are can be let go again (`forget_before`).
///
/// Pictures are kept as files of their own (`crate::blobs`): a message
/// added or changed has its pictures' data moved out at the next change
/// or save.
pub struct Messages {
    win: std::sync::OnceLock<Window>,
    disk: std::sync::Arc<std::sync::Mutex<Disk>>,
    /// How many messages there are, while none is read.
    stored: usize,
    /// The last message, read alone while the window isn't (`last`).
    tail: std::sync::OnceLock<Value>,
    /// The first `saved` messages are as the last save (or the file) has
    /// them.
    saved: std::sync::atomic::AtomicUsize,
    /// Saves made (the writer puts the last one written in `Disk::written`).
    queued: std::sync::atomic::AtomicU64,
    /// Messages from here may hold picture data (`blobs::externalize`).
    norm_from: usize,
    /// Looked at in the file (`load_conversations_then`): the first message
    /// with a call left without a result (`agent::answer_lost_calls`), and
    /// the first with a rewrite marker (`unslop::start`). `Some(0)` when not
    /// looked at.
    pub(crate) lost_from: Option<usize>,
    pub(crate) rewrites_from: Option<usize>,
    /// What the messages before a `base` say of kits loaded
    /// (`prefix::loads_of`), kept while nothing before it changes.
    pub(crate) loads_memo: std::sync::Mutex<Option<(usize, crate::prefix::Loads)>>,
}

/// How many times messages were read from disk (the frame benchmark).
#[cfg(test)]
pub(crate) static PARSED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Default for Messages {
    fn default() -> Messages {
        Vec::new().into()
    }
}

fn lock_disk(d: &std::sync::Mutex<Disk>) -> std::sync::MutexGuard<'_, Disk> {
    d.lock().unwrap_or_else(|e| e.into_inner())
}

impl Messages {
    /// Messages kept in a file, at `ranges` in it (none read yet).
    fn stored(ranges: Vec<(u64, u64)>) -> Messages {
        let n = ranges.len();
        Messages {
            win: std::sync::OnceLock::new(),
            disk: std::sync::Arc::new(std::sync::Mutex::new(Disk { ranges, ..Default::default() })),
            stored: n,
            tail: std::sync::OnceLock::new(),
            saved: n.into(),
            queued: 0.into(),
            norm_from: n,
            lost_from: Some(0),
            rewrites_from: Some(0),
            loads_memo: Default::default(),
        }
    }

    /// The window, read when first needed: the last messages.
    fn win(&self) -> &Window {
        self.win.get_or_init(|| {
            let disk = lock_disk(&self.disk);
            let mut base = window_start(&disk.ranges, self.stored);
            let mut tail = disk.messages(base, self.stored);
            // (Not starting at results without their calls.)
            while base > 0 && tail.first().is_some_and(results_only) {
                base -= 1;
                tail.insert(0, disk.messages(base, base + 1).pop().unwrap_or_default());
            }
            let mut list = vec![Value::Null; base];
            list.append(&mut tail);
            Window { base, list }
        })
    }

    /// Whether any message has been read (or made here).
    pub fn loaded(&self) -> bool {
        self.win.get().is_some()
    }

    /// The first message held (the ones before it are on disk only); the
    /// message count when none is read.
    pub fn base(&self) -> usize {
        self.win.get().map_or(self.stored, |w| w.base)
    }

    pub fn len(&self) -> usize {
        self.win.get().map_or(self.stored, |w| w.list.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The last message, reading only it when none is read yet.
    pub fn last(&self) -> Option<&Value> {
        if let Some(w) = self.win.get() {
            return w.list.last();
        }
        let n = self.stored.checked_sub(1)?;
        Some(self.tail.get_or_init(|| lock_disk(&self.disk).messages(n, n + 1).pop().unwrap_or_default()))
    }

    /// Reads the messages from `i` on that aren't held yet.
    pub fn load_from(&mut self, i: usize) {
        let base = self.win().base;
        if i >= base {
            return;
        }
        let got = lock_disk(&self.disk).messages(i, base);
        let w = self.win.get_mut().unwrap();
        for (slot, m) in w.list[i..base].iter_mut().zip(got) {
            *slot = m;
        }
        w.base = i;
    }

    /// Every message, read.
    pub fn load_all(&mut self) {
        self.load_from(0);
    }

    /// Reads the next earlier part (`window_start`), not starting at
    /// results without their calls. False when everything is read.
    pub fn load_earlier(&mut self) -> bool {
        let base = self.win().base;
        if base == 0 {
            return false;
        }
        let mut from = window_start(&lock_disk(&self.disk).ranges, base).min(base - 1);
        self.load_from(from);
        while from > 0 && results_only(&self.win().list[from]) {
            from -= 1;
            self.load_from(from);
        }
        true
    }

    /// Up to where (`k` at most) messages could be let go: the file has
    /// them as they are (every save is written, and none of them changed
    /// since). Not between a call and its results.
    pub fn forgettable(&self, k: usize) -> Option<usize> {
        let w = self.win.get()?;
        let mut k = k.min(self.saved.load(std::sync::atomic::Ordering::SeqCst)).min(w.list.len());
        while k > w.base && k < w.list.len() && results_only(&w.list[k]) {
            k -= 1;
        }
        if k <= w.base || !self.on_disk(k) {
            return None;
        }
        Some(k)
    }

    /// Whether the file holds the first `k` messages as they are now.
    fn on_disk(&self, k: usize) -> bool {
        let d = lock_disk(&self.disk);
        !d.lost && d.path.is_some() && d.ranges.len() >= k && k <= self.saved.load(std::sync::atomic::Ordering::SeqCst) && d.written == self.queued.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Lets the messages before `k` go when they can be (`forgettable`).
    /// True when it did.
    pub fn forget_before(&mut self, k: usize) -> bool {
        let Some(k) = self.forgettable(k) else { return false };
        let w = self.win.get_mut().unwrap();
        // A rewrite still to do in what goes is found again (`unslop::start`).
        if let Some(i) = (w.base..k).find(|&i| crate::unslop::has_marker(&w.list[i])) {
            self.rewrites_from = Some(self.rewrites_from.map_or(i, |r| r.min(i)));
        }
        for m in &mut w.list[w.base..k] {
            *m = Value::Null;
        }
        w.base = k;
        true
    }

    /// Whether every message could be let go (`unload`).
    pub fn unloadable(&self) -> bool {
        self.loaded() && self.on_disk(self.len())
    }

    /// Lets every message go, as when the conversation was read at start,
    /// when the file has them as they are. True when it did.
    pub fn unload(&mut self) -> bool {
        if !self.unloadable() {
            return false;
        }
        let n = self.len();
        let w = self.win.take().unwrap();
        if let Some(i) = (w.base..n).find(|&i| crate::unslop::has_marker(&w.list[i])) {
            self.rewrites_from = Some(self.rewrites_from.map_or(i, |r| r.min(i)));
        }
        self.stored = n;
        self.tail = std::sync::OnceLock::new();
        self.norm_from = n;
        true
    }

    /// Where the window of the last messages starts (as they are on disk).
    pub fn tail_start(&self) -> usize {
        let d = lock_disk(&self.disk);
        let n = self.len().min(d.ranges.len());
        window_start(&d.ranges, n)
    }

    /// Messages `from..to` without holding them: those held are copied,
    /// the rest read from disk.
    pub fn read_range(&self, from: usize, to: usize) -> Vec<Value> {
        let to = to.min(self.len());
        if from >= to {
            return vec![];
        }
        let base = self.base();
        let mut out = if from < base { lock_disk(&self.disk).messages(from, base.min(to)) } else { vec![] };
        if let Some(held) = self.win.get().and_then(|w| w.list.get(base.max(from).min(to)..to)) {
            out.extend(held.iter().cloned());
        }
        out
    }

    /// Goes through messages `0..to` that aren't held a part at a time,
    /// read from disk, with the index of each part's first.
    pub fn each_on_disk(&self, to: usize, mut f: impl FnMut(usize, &[Value])) {
        const PART: usize = 200;
        let to = to.min(self.base());
        for from in (0..to).step_by(PART) {
            let part = lock_disk(&self.disk).messages(from, (from + PART).min(to));
            f(from, &part);
        }
    }

    /// The message at `i` and those after it are to change: they are read,
    /// pictures added since the last change are moved to files, and what
    /// is as saved ends before `i`.
    fn changed(&mut self, i: usize) -> &mut Vec<Value> {
        self.load_from(i);
        self.forget_memos(i);
        let from = self.norm_from;
        let w = self.win.get_mut().unwrap();
        for m in w.list.iter_mut().skip(from) {
            crate::blobs::externalize(m);
        }
        let n = w.list.len();
        self.norm_from = i.min(n);
        self.saved.fetch_min(i.min(n), std::sync::atomic::Ordering::SeqCst);
        &mut self.win.get_mut().unwrap().list
    }

    /// What is kept of the messages before a base goes if message `i` is
    /// among them.
    fn forget_memos(&mut self, i: usize) {
        let memo = self.loads_memo.get_mut().unwrap_or_else(|e| e.into_inner());
        if memo.as_ref().is_some_and(|(at, _)| *at > i) {
            *memo = None;
        }
    }

    pub fn push(&mut self, mut m: Value) {
        crate::blobs::externalize(&mut m);
        let n = self.len();
        self.changed(n).push(m);
        self.norm_from = n + 1;
    }

    pub fn truncate(&mut self, n: usize) {
        let base = self.win().base;
        if n < base {
            // (Read back to a window's worth before the cut.)
            let w = self.win.get_mut().unwrap();
            w.list.truncate(n);
            w.base = n;
            self.forget_memos(n);
            self.saved.fetch_min(n, std::sync::atomic::Ordering::SeqCst);
            self.norm_from = self.norm_from.min(n);
            let from = window_start(&lock_disk(&self.disk).ranges, n);
            self.load_from(from);
            return;
        }
        self.changed(n).truncate(n);
    }

    pub fn last_mut(&mut self) -> Option<&mut Value> {
        let n = self.len();
        self.changed(n.saturating_sub(1)).last_mut()
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.changed(i).get_mut(i)
    }

    /// Every message (all read), to change any of them.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Value> {
        self.changed(0).iter_mut()
    }

    /// What a save writes for each message: the JSON of those changed since
    /// the last, the rest copied from the file. The next save counts them
    /// as saved.
    fn entries(&self) -> (Vec<Entry>, u64) {
        use std::sync::atomic::Ordering::SeqCst;
        let saved = self.saved.load(SeqCst);
        let entries = match self.win.get() {
            None => (0..self.stored).map(Entry::Disk).collect(),
            Some(w) => {
                let mut buf = vec![];
                (0..w.list.len())
                    .map(|i| {
                        if i < saved {
                            return Entry::Disk(i);
                        }
                        buf.clear();
                        let m = &w.list[i];
                        if crate::blobs::has_inline(m) {
                            let mut m = m.clone();
                            crate::blobs::externalize(&mut m);
                            serde_json::to_writer(&mut buf, &m).unwrap();
                        } else {
                            serde_json::to_writer(&mut buf, m).unwrap();
                        }
                        Entry::Bytes(buf.as_slice().into())
                    })
                    .collect()
            }
        };
        self.saved.store(self.len(), SeqCst);
        (entries, self.queued.fetch_add(1, SeqCst) + 1)
    }

    /// Every message, read (those not held, from disk).
    fn all(&self) -> Vec<Value> {
        self.read_range(0, self.len())
    }
}

impl From<Vec<Value>> for Messages {
    fn from(list: Vec<Value>) -> Messages {
        Messages {
            win: Window { base: 0, list }.into(),
            disk: Default::default(),
            stored: 0,
            tail: std::sync::OnceLock::new(),
            saved: 0.into(),
            queued: 0.into(),
            norm_from: 0,
            lost_from: Some(0),
            rewrites_from: Some(0),
            loads_memo: Default::default(),
        }
    }
}

impl std::ops::Deref for Messages {
    type Target = Vec<Value>;
    fn deref(&self) -> &Vec<Value> {
        &self.win().list
    }
}

impl std::ops::IndexMut<usize> for Messages {
    fn index_mut(&mut self, i: usize) -> &mut Value {
        &mut self.changed(i)[i]
    }
}

impl<I: std::slice::SliceIndex<[Value]>> std::ops::Index<I> for Messages {
    type Output = I::Output;
    fn index(&self, i: I) -> &I::Output {
        &self.win().list[i]
    }
}

/// A copy holds every message (read from disk as needed) and has no file
/// of its own until saved.
impl Clone for Messages {
    fn clone(&self) -> Messages {
        let mut m: Messages = self.all().into();
        (m.lost_from, m.rewrites_from) = (self.lost_from, self.rewrites_from);
        m
    }
}

impl<'a> IntoIterator for &'a Messages {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;
    fn into_iter(self) -> Self::IntoIter {
        self.win().list.iter()
    }
}

impl PartialEq for Messages {
    fn eq(&self, other: &Messages) -> bool {
        self.len() == other.len() && self.all() == other.all()
    }
}

impl PartialEq<Vec<Value>> for Messages {
    fn eq(&self, other: &Vec<Value>) -> bool {
        self.len() == other.len() && self.all() == *other
    }
}

impl PartialEq<Messages> for [Value] {
    fn eq(&self, other: &Messages) -> bool {
        other.len() == self.len() && other.all() == self
    }
}

impl std::fmt::Debug for Messages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.all().fmt(f)
    }
}

thread_local! {
    /// `Conversation::save` is serializing everything but the messages,
    /// which it adds from their saved JSON.
    static WITHOUT_MESSAGES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A conversation file is being read (`read_conversation`): where its
    /// bytes are, so the messages are only located, not parsed.
    static READING: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
}

impl Serialize for Messages {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        if WITHOUT_MESSAGES.get() {
            return [(); 0].serialize(s);
        }
        let mut seq = s.serialize_seq(Some(self.len()))?;
        for m in &self.all() {
            seq.serialize_element(m)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Messages {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Messages, D::Error> {
        let Some((at, len)) = READING.get() else {
            return Ok(Vec::<Value>::deserialize(d)?.into());
        };
        let raw = Vec::<&'de serde_json::value::RawValue>::deserialize(d)?;
        let mut ranges = Vec::with_capacity(raw.len());
        for m in raw {
            let start = (m.get().as_ptr() as usize).wrapping_sub(at);
            if start + m.get().len() > len {
                return Err(serde::de::Error::custom("a message outside the file"));
            }
            ranges.push((start as u64, (start + m.get().len()) as u64));
        }
        Ok(Messages::stored(ranges))
    }
}

/// A message in a save: its JSON, or the file's copy of it as the save
/// before had it (`Messages::entries`).
#[derive(Clone)]
enum Entry {
    Bytes(std::sync::Arc<[u8]>),
    Disk(usize),
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
    /// A chat: conversation only, no project. It has no working folder,
    /// its own system prompt (`agent::prompt::CHAT`) and only the tools
    /// in `tools::CHAT_TOOLS` (web search and fetch, ASK, notes): no file,
    /// command, browser or subagent tool, refused at run time too
    /// (`tool_calls::run_call`). Unset, a coding conversation.
    #[serde(default, skip_serializing_if = "is_default")]
    pub chat: bool,
    /// The transcript, in the Oak format (`crate::oak`): only ever
    /// appended to.
    pub messages: Messages,
    /// The format `messages` is in: a file without one is from before the
    /// Oak format, in the Anthropic Messages format (`Conversation::upgrade`).
    #[serde(default = "Format::old")]
    pub format: Format,
    pub last_message_at: i64,
    /// Its last progress (`agent::pulse`): when its model's last reply or
    /// tool result went in (ms since 1970; 0 none yet), and what it was.
    #[serde(default, skip_serializing_if = "is_default")]
    pub progress_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,
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
    /// The user made the goal endless: GOAL_JUSTIFICATION and GOAL_FINISH
    /// are refused, and the goal lasts until the user pauses or clears it.
    #[serde(default, skip_serializing_if = "is_default")]
    pub goal_endless: bool,
    /// The user paused the goal: it stays (and the model still has it), but
    /// OAK doesn't ask the model to carry on with it when it ends its turn.
    /// Only OAK knows; nothing about it is sent.
    #[serde(default)]
    pub goal_paused: bool,
    /// Questions the model asked with ASK while a goal was active, waiting
    /// for the user's answers (the ASK's input; null when none). The model
    /// carried on meanwhile; the answers reach it as a message
    /// (`agent::Session::answer_questions`). A newer ASK replaces them.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub pending_ask: Value,
    /// An older Oak's REQUEST_ASK waiting for the user (what the model
    /// wanted to ask about), and whether the user accepted one. Read only,
    /// to tell the model it is gone (`agent::retire_request_ask`).
    #[serde(default, skip_serializing)]
    pub ask_request: String,
    #[serde(default, skip_serializing)]
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
    /// What requests went with before each summary in force acted, the
    /// latest last (`agent::install_summary`): a revert to before a
    /// summary's cut puts back what was sent before it, exactly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<Earlier>,
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
    /// A pause waiting for the request under way to end
    /// (`Session::request_pause`): if OAK closes first, the conversation
    /// starts paused, as it would have been.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pause_queued: bool,
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
    /// The TASK waits under way, by call: a wait Oak closed on carries on
    /// at the next start, for what was left of its timeout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_waits: Vec<TaskWait>,
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

/// A TASK wait under way (`Conversation::task_waits`). Its time counts only
/// while Oak runs: one Oak closed on goes on at the next start for the rest
/// of its timeout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskWait {
    /// The TASK call's id.
    pub call: String,
    /// When the wait began, or carried on after a restart (ms since 1970).
    pub since: i64,
    /// How long it had waited before `since`.
    pub waited_ms: i64,
    /// How long Oak was closed during it, all told.
    pub closed_ms: i64,
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
            self.messages.load_all();
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
    /// `Settings::unslop_notes_epoch` when it was made: one from an earlier
    /// epoch is dropped and made again (`unslop::start_notes`).
    #[serde(default, skip_serializing_if = "is_default")]
    pub epoch: u32,
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
    /// For a limit: the model whose own quota it is ("Fable") when the
    /// accounts still had room for other models; empty for a plan-wide limit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

impl Mark {
    /// A line at `at`, not done, sent with nothing.
    pub fn new(at: usize, kind: MarkKind, text: impl Into<String>, time: i64) -> Self {
        Mark { at, kind, text: text.into(), time, done: 0, sent: String::new(), scope: String::new() }
    }
}

/// The session memory block in the system prompt. It changes only when the
/// prompt cache is being rebuilt anyway (a summary was applied, or the cache
/// went cold), so notes don't cost a rewrite of the whole context.
/// What a conversation's requests went with up to a summary: the summary
/// before it, the prefix and the session memory as sent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Earlier {
    pub summary: Option<crate::rolling::Summary>,
    pub cold: crate::rolling::Clearing,
    pub measured: Vec<crate::rolling::Measure>,
    pub prefix: crate::prefix::State,
    pub memory: Option<Memory>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// messages changed since the last save serialized (the rest copied
    /// from the file as it is written).
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
        let (messages, seq) = self.messages.entries();
        Data { before: rest, messages, after, disk: self.messages.disk.clone(), seq }
    }
}

/// A conversation's file: its messages' JSON, comma separated, between
/// `before` and `after`; `disk` is where the messages it copies are, and
/// it is the conversation's save `seq`.
struct Data {
    before: Vec<u8>,
    messages: Vec<Entry>,
    after: Vec<u8>,
    disk: std::sync::Arc<std::sync::Mutex<Disk>>,
    seq: u64,
}

impl Data {
    fn len(&self) -> usize {
        self.before.len() + self.messages.iter().map(|m| if let Entry::Bytes(b) = m { b.len() + 1 } else { 1 }).sum::<usize>() + self.after.len()
    }

    /// Takes the messages that a save made before this one (`earlier`, of
    /// the same conversation, never written) has changed, where this one
    /// would copy them from the file.
    fn carry(&mut self, earlier: &Data) {
        if !std::sync::Arc::ptr_eq(&self.disk, &earlier.disk) {
            return;
        }
        for e in &mut self.messages {
            if let Entry::Disk(i) = *e
                && let Some(was) = earlier.messages.get(i)
            {
                *e = was.clone();
            }
        }
    }

    /// Writes the file to `out`, copying from `old` (the file as `ranges`
    /// has it) what isn't new. Each message's place in what is written.
    fn write_to(&self, out: &mut impl std::io::Write, mut old: Option<(&mut std::fs::File, &[(u64, u64)])>) -> std::io::Result<Vec<(u64, u64)>> {
        use std::io::{Read, Seek};
        let mut at = self.before.len() as u64;
        out.write_all(&self.before)?;
        let mut ranges = Vec::with_capacity(self.messages.len());
        let mut i = 0;
        while i < self.messages.len() {
            if i > 0 {
                out.write_all(b",")?;
                at += 1;
            }
            match &self.messages[i] {
                Entry::Bytes(b) => {
                    out.write_all(b)?;
                    ranges.push((at, at + b.len() as u64));
                    at += b.len() as u64;
                    i += 1;
                }
                // A run of the file's messages in order: copied in one go.
                Entry::Disk(first) => {
                    let mut last = *first;
                    while matches!(self.messages.get(i + 1 + last - first), Some(Entry::Disk(n)) if *n == last + 1) {
                        last += 1;
                    }
                    let Some((f, old)) = old.as_mut().filter(|(_, r)| last < r.len()) else {
                        return Err(std::io::Error::other("a message that isn't on disk"));
                    };
                    let (start, end) = (old[*first].0, old[last].1);
                    f.seek(std::io::SeekFrom::Start(start))?;
                    let copied = std::io::copy(&mut (&mut **f).take(end - start), out)?;
                    if copied != end - start {
                        return Err(std::io::Error::other("the file is shorter than it was"));
                    }
                    ranges.extend(old[*first..=last].iter().map(|&(a, b)| (a - start + at, b - start + at)));
                    at += end - start;
                    i += last - first + 1;
                }
            }
        }
        out.write_all(&self.after)?;
        Ok(ranges)
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

/// The copies of each conversation (by id) whose messages its file holds:
/// one that writes it leaves the others with nothing on disk
/// (`Disk::lost`), as their places in it are no longer right.
static OWNERS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Vec<DiskRef>>>> = std::sync::LazyLock::new(Default::default);

type DiskRef = std::sync::Weak<std::sync::Mutex<Disk>>;

/// `disk` wrote conversation `id`'s file: the other copies lose theirs.
fn own(id: &str, disk: &std::sync::Arc<std::sync::Mutex<Disk>>) {
    let others = OWNERS.lock().unwrap().insert(id.to_string(), vec![std::sync::Arc::downgrade(disk)]);
    for other in others.into_iter().flatten().filter_map(|w| w.upgrade()) {
        if !std::sync::Arc::ptr_eq(&other, disk) {
            lock_disk(&other).lost = true;
        }
    }
}

/// `disk` was read from conversation `id`'s file, as it is now.
fn read_from(id: &str, disk: &std::sync::Arc<std::sync::Mutex<Disk>>) {
    let mut owners = OWNERS.lock().unwrap();
    let copies = owners.entry(id.to_string()).or_default();
    copies.retain(|w| w.strong_count() > 0);
    copies.push(std::sync::Arc::downgrade(disk));
}

/// The writer thread: writes the saves in the order they were made, only
/// the newest of a conversation's when several wait (with what those
/// before it changed). A save that couldn't be written is carried into the
/// conversation's next one the same way.
fn write_conversations() {
    let (lock, wake) = &*QUEUE;
    let mut failed: std::collections::HashMap<String, Data> = Default::default();
    loop {
        let mut writes: Vec<Option<Write>> = {
            let mut q = wake.wait_while(lock.lock().unwrap(), |q| q.writes.is_empty()).unwrap();
            q.busy = true;
            std::mem::take(&mut q.writes).into_iter().map(Some).collect()
        };
        for i in 0..writes.len() {
            let Some(mut w) = writes[i].take() else { continue };
            if let Some(earlier) = failed.remove(&w.id) {
                w.data.carry(&earlier);
            }
            if let Some(later) = writes[i + 1..].iter_mut().flatten().find(|l| l.id == w.id) {
                later.data.carry(&w.data);
                crate::bench::log(&w.id, "save_superseded", serde_json::json!({"from": format!("{}:{}", w.from.file(), w.from.line())}));
                continue;
            }
            if let Err(e) = write_conversation(&w) {
                eprintln!("Couldn't save conversation {}: {e}", w.id);
                failed.insert(w.id, w.data);
            }
        }
        lock.lock().unwrap().busy = false;
        wake.notify_all();
    }
}

fn write_conversation(w: &Write) -> std::io::Result<()> {
    let conversations = conversations_dir();
    let path = conversations.join(&w.name);
    let tmp = conversations.join(format!("{}.tmp", w.id));
    let copies = w.data.messages.iter().any(|e| matches!(e, Entry::Disk(_)));
    let (old_path, old_ranges, lost) = {
        let d = lock_disk(&w.data.disk);
        (d.path.clone(), if copies { d.ranges.clone() } else { vec![] }, d.lost)
    };
    if copies && lost {
        return Err(std::io::Error::other("another copy of the conversation wrote its file"));
    }
    // (Only the writer changes the file: it is read without the lock.)
    let mut old = match (copies, old_path) {
        (false, _) => None,
        (true, Some(p)) => Some(std::fs::File::open(p)?),
        (true, None) => return Err(std::io::Error::other("no file to copy messages from")),
    };
    let written = std::fs::File::create(&tmp).and_then(|f| {
        let mut out = std::io::BufWriter::with_capacity(1 << 16, f);
        let ranges = w.data.write_to(&mut out, old.as_mut().map(|f| (f, old_ranges.as_slice())))?;
        std::io::Write::flush(&mut out)?;
        Ok(ranges)
    });
    drop(old);
    let renamed = written.and_then(|ranges| {
        let mut d = lock_disk(&w.data.disk);
        std::fs::rename(&tmp, &path)?;
        (d.path, d.ranges, d.written) = (Some(path), ranges, w.data.seq);
        Ok(())
    });
    if let Err(e) = renamed {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    own(&w.id, &w.data.disk);
    let old = FILES.lock().unwrap().insert(w.id.clone(), w.name.clone());
    let old = old.unwrap_or_else(|| format!("{}.json", w.id));
    if old != w.name {
        let _ = std::fs::remove_file(conversations.join(old));
    }
    crate::bench::log(&w.id, "save", serde_json::json!({"ms": w.started.elapsed().as_secs_f64() * 1000.0, "bytes": w.data.len(), "from": format!("{}:{}", w.from.file(), w.from.line())}));
    Ok(())
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

/// What `look` is given of a conversation as it is read: each message's
/// JSON, as the file has it.
pub type Look = dyn Fn(&mut Conversation, &[&[u8]]) + Sync;

/// Reads a conversation file: everything but its messages, which are only
/// located (`Messages::stored`), and `look` done to it while its bytes
/// are at hand. Pictures still in it as data are moved to their files
/// first (`blobs`), and the file written again without them, once.
#[cfg(test)]
pub(crate) fn read_conversation(path: &std::path::Path, look: &Look) -> Option<Conversation> {
    read_file(path, look).map(|r| r.0)
}

/// `read_conversation`, and what the index keeps of it.
fn read_file(path: &std::path::Path, look: &Look) -> Option<(Conversation, Indexed)> {
    let buf = std::fs::read(path).ok()?;
    READING.set(Some((buf.as_ptr() as usize, buf.len())));
    let c = serde_json::from_slice::<Conversation>(&buf);
    READING.set(None);
    let mut c = c.ok()?;
    let ranges = lock_disk(&c.messages.disk).ranges.clone();
    let raw: Vec<&[u8]> = ranges.iter().map(|&(a, b)| &buf[a as usize..b as usize]).collect();
    if c.format.0 > 0 && raw.iter().any(|m| may_hold_picture(m)) {
        let moved: Vec<Option<Vec<u8>>> = raw
            .iter()
            .map(|m| {
                let mut v: Value = serde_json::from_slice(m).ok().filter(|_| may_hold_picture(m))?;
                crate::blobs::externalize(&mut v).then(|| serde_json::to_vec(&v).unwrap())
            })
            .collect();
        if moved.iter().any(Option::is_some) {
            let mut out = Vec::with_capacity(buf.len() / 4);
            let mut at = 0;
            for (&(a, b), m) in ranges.iter().zip(&moved) {
                if let Some(m) = m {
                    out.extend_from_slice(&buf[at..a as usize]);
                    out.extend_from_slice(m);
                    at = b as usize;
                }
            }
            out.extend_from_slice(&buf[at..]);
            // (Its time kept: of two files of one conversation, the one
            // saved last is the one kept.)
            let modified = std::fs::metadata(path).and_then(|m| m.modified());
            if write_atomic(path, &out).is_ok() {
                if let Ok(t) = modified {
                    let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(t));
                }
                drop(out);
                drop(raw);
                drop(buf);
                return read_file(path, look);
            }
        }
    }
    lock_disk(&c.messages.disk).path = Some(path.to_path_buf());
    look(&mut c, &raw);
    // The file without its messages' JSON.
    let meta = match (ranges.first(), ranges.last()) {
        (Some(&(first, _)), Some(&(_, last))) => [&buf[..first as usize], &buf[last as usize..]].concat(),
        _ => buf.clone(),
    };
    let indexed = Indexed { meta, ranges, lost_from: c.messages.lost_from, rewrites_from: c.messages.rewrites_from };
    Some((c, indexed))
}

/// What the index (`INDEX`) keeps of a conversation file, to start without
/// reading it again while it is unchanged: the file without its messages,
/// where each message is in it, and what `look` found in them.
struct Indexed {
    meta: Vec<u8>,
    ranges: Vec<(u64, u64)>,
    lost_from: Option<usize>,
    rewrites_from: Option<usize>,
}

impl Indexed {
    /// The conversation it was made from (at `path`), its messages left on
    /// disk.
    fn conversation(&self, path: &std::path::Path) -> Option<Conversation> {
        READING.set(Some((self.meta.as_ptr() as usize, self.meta.len())));
        let c = serde_json::from_slice::<Conversation>(&self.meta);
        READING.set(None);
        let mut c = c.ok().filter(|c| c.messages.is_empty())?;
        c.messages = Messages::stored(self.ranges.clone());
        lock_disk(&c.messages.disk).path = Some(path.to_path_buf());
        (c.messages.lost_from, c.messages.rewrites_from) = (self.lost_from, self.rewrites_from);
        Some(c)
    }
}

/// The index of the conversation files, in their folder: for each file
/// (by name, size and time) what `Indexed` keeps, so that a start reads
/// only the files changed since the last. Written after each start, in
/// the background; a cache only (any file it doesn't match is read).
const INDEX: &str = "index.bin";
const INDEX_MAGIC: &[u8] = b"OAKINDEX1\n";

/// A file's size and time, as the index tells them apart.
fn stamp(e: &std::fs::DirEntry) -> Option<(u64, u64)> {
    let m = e.metadata().ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((m.len(), t.as_nanos() as u64))
}

type Index = std::collections::HashMap<String, ((u64, u64), Indexed)>;

fn read_index() -> Index {
    std::fs::read(conversations_dir().join(INDEX)).map(|b| parse_index(&b)).unwrap_or_default()
}

/// The index from its bytes (`index_bytes`); empty if they don't read.
fn parse_index(buf: &[u8]) -> Index {
    let mut out = std::collections::HashMap::new();
    let Some(mut r) = buf.strip_prefix(INDEX_MAGIC) else { return out };
    fn take<'a>(r: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (a, b) = r.split_at_checked(n)?;
        *r = b;
        Some(a)
    }
    let u64_ = |r: &mut &[u8]| take(r, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let u32_ = |r: &mut &[u8]| take(r, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let opt = |v: u64| (v != u64::MAX).then_some(v as usize);
    let entry = |r: &mut &[u8]| -> Option<(String, (u64, u64), Indexed)> {
        let n = u32_(r)? as usize;
        let name = String::from_utf8(take(r, n)?.to_vec()).ok()?;
        let stamp = (u64_(r)?, u64_(r)?);
        let (lost_from, rewrites_from) = (opt(u64_(r)?), opt(u64_(r)?));
        let n = u32_(r)? as usize;
        let meta = take(r, n)?.to_vec();
        let n = u32_(r)? as usize;
        let mut ranges = Vec::with_capacity(n);
        let mut at = u64_(r)?;
        for _ in 0..n {
            let (gap, len) = (u32_(r)? as u64, u32_(r)? as u64);
            ranges.push((at + gap, at + gap + len));
            at += gap + len;
        }
        Some((name, stamp, Indexed { meta, ranges, lost_from, rewrites_from }))
    };
    while !r.is_empty() {
        let Some((name, stamp, i)) = entry(&mut r) else { return Default::default() };
        out.insert(name, (stamp, i));
    }
    out
}

fn write_index(entries: &[(String, (u64, u64), std::sync::Arc<Indexed>)]) {
    let _ = write_atomic_unsynced(&conversations_dir().join(INDEX), index_bytes(entries));
}

fn index_bytes(entries: &[(String, (u64, u64), std::sync::Arc<Indexed>)]) -> Vec<u8> {
    let mut out = INDEX_MAGIC.to_vec();
    let opt = |v: Option<usize>| v.map_or(u64::MAX, |v| v as u64);
    for (name, (size, time), i) in entries {
        out.extend((name.len() as u32).to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(size.to_le_bytes());
        out.extend(time.to_le_bytes());
        out.extend(opt(i.lost_from).to_le_bytes());
        out.extend(opt(i.rewrites_from).to_le_bytes());
        out.extend((i.meta.len() as u32).to_le_bytes());
        out.extend(&i.meta);
        out.extend((i.ranges.len() as u32).to_le_bytes());
        out.extend(0u64.to_le_bytes());
        let mut at = 0;
        for &(a, b) in &i.ranges {
            out.extend(((a - at) as u32).to_le_bytes());
            out.extend(((b - a) as u32).to_le_bytes());
            at = b;
        }
    }
    out
}

/// Whether a message's JSON may hold a picture's data.
fn may_hold_picture(m: &[u8]) -> bool {
    use memchr::memmem::find;
    find(m, br#""type":"image""#).is_some() && find(m, br#""data":""#).is_some()
}

/// Reads conversation files on every core, the biggest first; in `files`'
/// order. Those unchanged since the index was written aren't read: the
/// index has what is needed of them (`Indexed`). The index is then written
/// again, in the background, when anything changed.
fn read_all(files: &[std::fs::DirEntry], look: &Look) -> Vec<Option<Conversation>> {
    let index = read_index();
    let stamps: Vec<Option<(u64, u64)>> = files.iter().map(stamp).collect();
    let names: Vec<String> = files.iter().map(|f| f.file_name().to_string_lossy().into_owned()).collect();
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(stamps[i].map_or(0, |s| s.0)));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let read = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(files.len()).max(1);
    let bytes: u64 = stamps.iter().flatten().map(|s| s.0).sum();
    type Got = (usize, Option<(Conversation, std::sync::Arc<Indexed>)>);
    let mut parsed: Vec<Got> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = vec![];
                    while let Some(&i) = order.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        let path = files[i].path();
                        let known = index.get(&names[i]).filter(|(st, _)| Some(*st) == stamps[i]).and_then(|(_, x)| Some((x.conversation(&path)?, x)));
                        let got = match known {
                            Some((c, x)) => Some((c, std::sync::Arc::new(Indexed { meta: x.meta.clone(), ranges: x.ranges.clone(), ..*x }))),
                            None => {
                                read.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                read_file(&path, look).map(|(c, x)| (c, std::sync::Arc::new(x)))
                            }
                        };
                        out.push((i, got));
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    });
    let read = read.into_inner();
    crate::startup::note(format!("Conversation files: {} ({:.1} MB), {read} read (the rest as indexed) on {threads} threads", files.len(), bytes as f64 / 1e6));
    parsed.sort_by_key(|p| p.0);
    // (Restamped: a file moved out of its pictures was written again.)
    let entries: Vec<(String, (u64, u64), std::sync::Arc<Indexed>)> =
        parsed.iter().filter_map(|(i, got)| Some((names[*i].clone(), stamp(&files[*i]).or(stamps[*i])?, got.as_ref()?.1.clone()))).collect();
    if read > 0 || entries.len() != index.len() {
        std::thread::spawn(move || write_index(&entries));
    }
    parsed.into_iter().map(|p| p.1.map(|g| g.0)).collect()
}

/// `load_conversations_then`, looking at nothing.
#[cfg(test)]
pub fn load_conversations() -> Vec<Conversation> {
    load_conversations_then(&|_, _| {})
}

/// Every saved conversation (its messages left on disk until looked at),
/// with `look` done to each as soon as it is read, on the thread that
/// read it (while its file's JSON is at hand). A conversation found under
/// two names (OAK stopped between writing the new file and removing the
/// old one) is loaded from the one saved last.
pub fn load_conversations_then(look: &Look) -> Vec<Conversation> {
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
        read_from(&id, &c.messages.disk);
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
    /// Counts the times rewritten notes stopped showing (Unslop or Rewrite
    /// notes turned off): the rewrites made before are dropped, and made
    /// again once both are on.
    pub unslop_notes_epoch: u32,
    /// The model is told to tidy up its notes once they pass
    /// `agent::notes_alert_tokens`.
    pub notes_alert: bool,
    /// The hard limit on a session's notes, in tokens (`agent::notes_limit`).
    pub notes_limit: u64,
    /// The OpenRouter model that names sessions (an id in
    /// `crate::openrouter::TITLE_CHOICES`).
    pub title_model: String,
    /// How OpenRouter requests pick a provider (an id of
    /// `crate::openrouter::Routing`).
    pub or_routing: String,
    /// The conversations paused (or ultrapaused) by closing Oak while they
    /// worked: the next start offers to resume them, once.
    pub paused_on_close: Vec<String>,
    /// Scratchpads idle this long (seconds) are emptied (`crate::scratch`);
    /// 0: never.
    pub scratch_clear: u64,
    /// How Home lists this machine's conversations: "" for the latest
    /// first, "scratchpad" for the biggest scratchpad first
    /// (`crate::home::ChatSort`). A string so an unknown value doesn't
    /// make the whole file unreadable.
    pub chat_sort: String,
    /// Home's wallpaper: "" for Oak's forest, else a file's name in the
    /// `wallpapers` folder (`crate::wallpaper`).
    pub wallpaper: String,
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
            rotation: true,
            zoom: 1.0,
            click_zoom: false,
            touchpad_speed: 3.0,
            tabs: vec![],
            unslop: false,
            unslop_no_drafts: false,
            unslop_thinking: false,
            unslop_notes: false,
            unslop_notes_epoch: 0,
            notes_alert: true,
            notes_limit: crate::agent::DEFAULT_NOTES_LIMIT,
            title_model: crate::openrouter::TITLE_CHOICES[0].0.into(),
            or_routing: "balanced".into(),
            paused_on_close: vec![],
            scratch_clear: 0,
            chat_sort: String::new(),
            wallpaper: String::new(),
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
    /// its messages changed since the last one (only those are redone, the
    /// rest copied from the file).
    #[test]
    fn save_writes_the_whole_conversation() {
        use serde_json::json;
        let mut c = super::Conversation {
            id: format!("unit-test-whole-{}", crate::api::now_ms()),
            title: "\"messages\":[] in a title".into(),
            messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "\"messages\":[1]"}]})].into(),
            ..Default::default()
        };
        let same = |c: &super::Conversation| {
            c.save();
            flush();
            let on_disk = std::fs::read_to_string(conversations_dir().join(c.file_name())).unwrap();
            assert_eq!(on_disk, serde_json::to_string(c).unwrap());
        };
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
        // Saves made faster than they are written: each carries what the
        // one before changed.
        for i in 0..5 {
            c.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": format!("quick {i}")}]}));
            c.save();
        }
        same(&c);
        let loaded = load_conversations().into_iter().find(|l| l.id == c.id).unwrap();
        assert!(!loaded.messages.loaded(), "only the metadata is read at start");
        assert_eq!(loaded.messages.len(), c.messages.len());
        assert_eq!(loaded.messages.last(), c.messages.last());
        assert_eq!(loaded.messages, c.messages);
        same(&loaded);
        remove_conversation(&c.id);
    }

    /// A conversation started from the index is the one its file holds,
    /// with what `look` found in it; a file that changed is read again.
    #[test]
    fn the_index_stands_for_unchanged_files() {
        use serde_json::json;
        let c = Conversation {
            id: format!("unit-test-index-{}", crate::api::now_ms()),
            title: "indexed".into(),
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "one"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "READ", "input": {}}]}),
            ]
            .into(),
            ..Default::default()
        };
        c.save();
        flush();
        let path = conversations_dir().join(c.file_name());
        let look = |c: &mut Conversation, raw: &[&[u8]]| crate::agent::look_for_lost_calls(c, raw);
        let (read, x) = read_file(&path, &look).unwrap();
        assert_eq!(read.messages.lost_from, Some(Some(1)).flatten());
        let entry = (c.file_name(), (7, 9), std::sync::Arc::new(x));
        let index = parse_index(&index_bytes(&[entry]));
        let ((size, time), x) = &index[&c.file_name()];
        assert_eq!((*size, *time), (7, 9));
        let from_index = x.conversation(&path).unwrap();
        assert!(!from_index.messages.loaded());
        assert_eq!(from_index.messages.lost_from, read.messages.lost_from);
        assert_eq!(serde_json::to_string(&from_index).unwrap(), serde_json::to_string(&c).unwrap());
        assert!(parse_index(b"OAKINDEX1\n\x05").is_empty(), "a broken index is no index");
        // Through `load_conversations`: the first start indexes it, the next
        // starts from the index; a change is read from the file.
        let load = || load_conversations().into_iter().find(|l| l.id == c.id).unwrap();
        assert_eq!(load().messages, c.messages);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(load().messages, c.messages);
        let mut changed = load();
        changed.messages.push(json!({"role": "user", "content": [{"type": "text", "text": "two"}]}));
        changed.save();
        flush();
        assert_eq!(load().messages.len(), 3);
        remove_conversation(&c.id);
    }

    /// An opened conversation holds its last messages; earlier ones are
    /// read as they are needed, and let go again once on disk as they are.
    #[test]
    fn messages_are_held_from_the_end() {
        use serde_json::json;
        let big = "x".repeat(10_000);
        let msg = |i: usize| match i % 2 {
            0 => json!({"role": "user", "content": [{"type": "text", "text": format!("{i} {big}")}]}),
            _ => json!({"role": "assistant", "content": [{"type": "text", "text": format!("{i}")}]}),
        };
        let all: Vec<Value> = (0..1000).map(msg).collect();
        let c = Conversation { id: format!("unit-test-window-{}", crate::api::now_ms()), messages: all.clone().into(), ..Default::default() };
        c.save();
        flush();
        let mut l = load_conversations().into_iter().find(|l| l.id == c.id).unwrap();
        assert_eq!((l.messages.len(), l.messages.base()), (1000, 1000));
        let base = l.messages.iter().count() - l.messages.iter().filter(|m| !m.is_null()).count();
        assert_eq!(base, l.messages.base());
        assert!((600..=1000 - WINDOW_MIN).contains(&base), "a window's worth: {base}");
        assert_eq!(l.messages[999], all[999]);
        assert!(l.messages[base - 1].is_null());
        assert!(l.messages.load_earlier());
        let earlier = l.messages.base();
        assert!(earlier < base && l.messages[earlier..] == all[earlier..]);
        assert_eq!(l.messages.read_range(0, 3), all[..3].to_vec());
        // A change to a message on disk only reads it first.
        l.messages[5]["content"][0]["text"] = json!("five");
        assert_eq!(l.messages.base(), 5);
        assert!(!l.messages.forget_before(900), "not on disk as it is yet");
        l.save();
        flush();
        assert!(l.messages.forget_before(900));
        assert_eq!(l.messages.base(), 900);
        assert!(l.messages[899].is_null());
        l.messages.push(msg(1000));
        l.save();
        flush();
        let mut want = all.clone();
        want[5]["content"][0]["text"] = json!("five");
        want.push(msg(1000));
        assert_eq!(l.messages, want);
        // Cut back before what is held: a window before the cut is read.
        l.messages.truncate(300);
        assert!(l.messages.base() < 300 && l.messages[299] == want[299]);
        l.save();
        flush();
        let again = load_conversations().into_iter().find(|a| a.id == c.id).unwrap();
        assert_eq!(again.messages, want[..300].to_vec());
        remove_conversation(&c.id);
    }

    /// Pictures saved in a transcript as data move to their files at the
    /// next start; what a request sends of them is as it was.
    #[test]
    fn pictures_move_out_of_transcripts() {
        use base64::Engine;
        use serde_json::json;
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255])));
        let mut png = std::io::Cursor::new(vec![]);
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        let id = format!("unit-test-pictures-{}", crate::api::now_ms());
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "media_type": "image/png", "data": data}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "seen"}]}),
        ];
        let mut v = serde_json::to_value(Conversation { id: id.clone(), ..Default::default() }).unwrap();
        v["messages"] = json!(msgs);
        let path = conversations_dir().join(format!("{id}.json"));
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let c = load_conversations().into_iter().find(|c| c.id == id).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&data) && text.contains("\"blob\""), "{text}");
        let b = &c.messages[0]["content"][1];
        assert_eq!(crate::blobs::data_of(b).unwrap(), data);
        let mut sent = c.messages.to_vec();
        crate::blobs::inline(&mut sent);
        assert_eq!(sent, msgs);
        remove_conversation(&id);
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
