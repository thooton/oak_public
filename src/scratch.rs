//! Sessions' scratchpads on disk (`.oak/tmp/tempN` in each working folder,
//! `agent::prompt::scratchpad`): how much they hold, clearing one, and
//! clearing those of sessions idle longer than a set age (Settings).
//!
//! A scratchpad's age is the later of its session's last message and the
//! newest file in it, so one a background task still writes to stays. A
//! `tempN` folder no conversation claims (its conversation was deleted) goes
//! by its files alone. Clearing empties the folder; the session makes it
//! again with its next request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// The ages Settings offers (seconds; 0: never).
pub const AGES: [(u64, &str); 6] = [
    (0, "Never"),
    (3_600, "1 hour"),
    (12 * 3_600, "12 hours"),
    (86_400, "1 day"),
    (7 * 86_400, "7 days"),
    (30 * 86_400, "1 month"),
];

/// A size as Settings and the conversation menu show it.
pub fn fmt_bytes(n: u64) -> String {
    match n {
        0 => "empty".into(),
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n => crate::transcript::size_text(n),
    }
}

/// A `tempN` folder: where it is, the conversation it belongs to, what it
/// holds, and when it last changed (ms since 1970).
#[derive(Clone, Debug)]
pub struct Pad {
    pub dir: PathBuf,
    pub owner: Option<String>,
    pub bytes: u64,
    pub last_ms: i64,
    /// Its session is at work, or has a background task running: never
    /// cleared on its own.
    pub in_use: bool,
}

/// The last scan of every scratchpad.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub pads: Vec<Pad>,
    pub at: Option<Instant>,
}

impl Scan {
    pub fn bytes(&self) -> u64 {
        self.pads.iter().map(|p| p.bytes).sum()
    }

    /// Bytes in conversation `id`'s scratchpad, as last scanned.
    pub fn of(&self, id: &str) -> Option<u64> {
        self.pads.iter().find(|p| p.owner.as_deref() == Some(id)).map(|p| p.bytes)
    }
}

static SCAN: Mutex<Scan> = Mutex::new(Scan { pads: vec![], at: None });
static SCANNING: AtomicBool = AtomicBool::new(false);
/// The age past which scratchpads are cleared (seconds; 0: never).
static AUTO: AtomicU64 = AtomicU64::new(0);

/// A session's scratchpad as the scan sees it: its conversation, folder,
/// `tempN`, last message, and whether it is in use.
pub struct Owner {
    pub id: String,
    pub folder: PathBuf,
    pub scratch: String,
    pub last_ms: i64,
    pub in_use: bool,
}

/// The last scan (scanning again in the background when it is older than
/// `stale`, or there is none).
pub fn scan(stale: Duration, ctx: &eframe::egui::Context) -> Scan {
    let s = SCAN.lock().unwrap().clone();
    if s.at.is_none_or(|at| at.elapsed() > stale) {
        rescan(Some(ctx.clone()));
    }
    s
}

/// Scans every scratchpad again in the background (one scan at a time).
pub fn rescan(ctx: Option<eframe::egui::Context>) {
    if SCANNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let pads = pads(&crate::agent::scratch_owners());
        *SCAN.lock().unwrap() = Scan { pads, at: Some(Instant::now()) };
        SCANNING.store(false, Ordering::SeqCst);
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    });
}

/// Every `tempN` folder in the working folders of `owners`, with what it
/// holds.
pub fn pads(owners: &[Owner]) -> Vec<Pad> {
    let mut by_dir: HashMap<PathBuf, &Owner> = HashMap::new();
    let mut roots: Vec<PathBuf> = vec![];
    for o in owners {
        let root = o.folder.join(crate::agent::SCRATCH_ROOT);
        if !o.scratch.is_empty() {
            by_dir.insert(root.join(&o.scratch), o);
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let mut out = vec![];
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let is_pad = name.to_str().and_then(|n| n.strip_prefix("temp")).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            // Folders only, never through a link.
            if !is_pad || !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let dir = e.path();
            let (bytes, newest) = walk(&dir);
            let owner = by_dir.get(&dir);
            out.push(Pad {
                bytes,
                last_ms: newest.max(owner.map_or(0, |o| o.last_ms)),
                owner: owner.map(|o| o.id.clone()),
                in_use: owner.is_some_and(|o| o.in_use),
                dir,
            });
        }
    }
    out
}

/// The bytes under `dir` and the newest change there (ms since 1970; the
/// folder's own when it is empty). Links are counted, not followed.
fn walk(dir: &Path) -> (u64, i64) {
    let ms = |m: &std::fs::Metadata| m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
    let mut newest = std::fs::symlink_metadata(dir).map_or(0, |m| ms(&m));
    let (mut bytes, mut stack) = (0, vec![dir.to_path_buf()]);
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            newest = newest.max(ms(&m));
            if m.is_dir() {
                stack.push(e.path());
            } else {
                bytes += m.len();
            }
        }
    }
    (bytes, newest)
}

/// Empties a scratchpad (the folder itself stays). Returns the bytes freed;
/// what couldn't be removed (a file open in a running program) stays.
pub fn clear(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let before = walk(dir).0;
    for e in entries.flatten() {
        let p = e.path();
        // (remove_dir_all removes a link to a folder, not what it points to.)
        let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
    before.saturating_sub(walk(dir).0)
}

/// Empties conversation `id`'s scratchpad. Returns the bytes freed.
pub fn clear_conversation(id: &str) -> u64 {
    let owners = crate::agent::scratch_owners();
    let Some(o) = owners.iter().find(|o| o.id == id && !o.scratch.is_empty()) else { return 0 };
    let freed = clear(&o.folder.join(crate::agent::SCRATCH_ROOT).join(&o.scratch));
    rescan(None);
    freed
}

/// The scratchpads to clear at `now` (ms) for `age` (seconds): idle that
/// long, not in use.
pub fn due(pads: &[Pad], age: u64, now: i64) -> Vec<&Pad> {
    if age == 0 {
        return vec![];
    }
    pads.iter().filter(|p| !p.in_use && p.bytes > 0 && now - p.last_ms > age as i64 * 1000).collect()
}

/// Sets the age past which scratchpads are cleared (seconds; 0: never),
/// and starts the clearing (once): soon after Oak starts, then every ten
/// minutes.
pub fn set_auto(age: u64) {
    AUTO.store(age, Ordering::Relaxed);
    static STARTED: std::sync::Once = std::sync::Once::new();
    if cfg!(test) || age == 0 {
        return;
    }
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(30));
            loop {
                auto_clear();
                std::thread::sleep(Duration::from_secs(600));
            }
        });
    });
}

fn auto_clear() {
    let age = AUTO.load(Ordering::Relaxed);
    if age == 0 {
        return;
    }
    let now = crate::api::now_ms();
    let all = pads(&crate::agent::scratch_owners());
    let mut cleared = false;
    for p in due(&all, age, now) {
        clear(&p.dir);
        cleared = true;
    }
    if cleared {
        rescan(None);
    } else {
        *SCAN.lock().unwrap() = Scan { pads: all, at: Some(Instant::now()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `tempN` folders count, a session's last message keeps its own
    /// scratchpad young, and one in use or empty is never due.
    #[test]
    fn scratchpads_due_for_clearing() {
        let folder = std::env::temp_dir().join(format!("oak-scratch-test-{}", std::process::id()));
        let root = folder.join(crate::agent::SCRATCH_ROOT);
        for (dir, file) in [("temp0", "a.txt"), ("temp1", "b.txt"), ("temp2", "c.txt"), ("other", "d.txt")] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(file), "12345").unwrap();
        }
        let now = crate::api::now_ms();
        let later = now + 3 * 3_600_000;
        let owner = |id: &str, scratch: &str, last_ms, in_use| Owner { id: id.into(), folder: folder.clone(), scratch: scratch.into(), last_ms, in_use };
        let owners = [owner("young", "temp0", later, false), owner("busy", "temp1", 0, true)];
        let all = pads(&owners);
        assert_eq!(all.len(), 3, "{all:?}");
        assert_eq!(all.iter().map(|p| p.bytes).sum::<u64>(), 15);
        // Two hours on: temp0's session wrote later, temp1's is at work,
        // temp2 (no conversation) is idle.
        let due: Vec<&Path> = due(&all, 3_600, now + 2 * 3_600_000).into_iter().map(|p| p.dir.as_path()).collect();
        assert_eq!(due, [root.join("temp2").as_path()]);
        assert_eq!(clear(&root.join("temp2")), 5);
        assert!(root.join("temp2").is_dir() && std::fs::read_dir(root.join("temp2")).unwrap().next().is_none());
        assert!(root.join("other").join("d.txt").exists());
        let _ = std::fs::remove_dir_all(&folder);
    }
}
