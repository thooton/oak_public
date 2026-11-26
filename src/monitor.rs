//! MONITOR (a kit, as Claude Code's Monitor tool): watches a command's
//! output in the background and sends the model each new line (or each
//! line matching a filter) as a task notification as it appears, so it
//! can react mid-conversation without waiting or polling. A watch runs a
//! command of its own (a task, `crate::tasks`) or follows a task the model
//! started. Lines that come close together (BATCH) go in one notification.
//! Every watch has a deadline (DEFAULT_MINUTES, at most MAX_MINUTES) unless
//! it is persistent; at the deadline it ends (its own command is stopped)
//! and the model gets one notice, to start it again if still needed. It
//! also ends when its command exits, when the model stops it, when it
//! is too chatty (MAX_PER_MINUTE) or when its session is gone. Watches
//! are the running Oak's only: they don't outlive it.

use crate::look::Head;
use crate::tasks::{self, Proc};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A watch's deadline when the model gives none, in minutes...
pub const DEFAULT_MINUTES: u64 = 5;
/// ...and the latest it may give.
pub const MAX_MINUTES: u64 = 30;
/// Lines that come within this of the first go in one notification.
const BATCH: Duration = Duration::from_millis(500);
/// The most lines and characters one notification shows (the rest are
/// counted, with the log file to read them in).
const MAX_LINES: usize = 50;
const MAX_CHARS: usize = 8_000;
/// More lines than this in a minute stop the watch: a firehose would
/// flood the model's context.
const MAX_PER_MINUTE: usize = 300;
/// How often a waiting watch checks for a stop or its deadline.
const TICK: Duration = Duration::from_millis(250);

/// Sends a notification's text to the session (wrapped as a task
/// notification there); false when the session is gone.
pub type Deliver = Arc<dyn Fn(String) -> bool + Send + Sync>;

/// What to watch.
pub struct Spec {
    /// The task whose output it is.
    pub task: u64,
    pub proc: Arc<Proc>,
    /// The watch started the command (and stops it when it ends).
    pub own: bool,
    /// What it watches, for the model: the summary or the command's start.
    pub what: String,
    /// `|`-separated alternatives, any of which a line must contain
    /// (case-insensitive); empty: every line.
    pub filter: String,
    /// None: persistent (no deadline).
    pub minutes: Option<u64>,
}

/// A watch under way.
struct Watch {
    id: u64,
    task: u64,
    own: bool,
    what: String,
    filter: String,
    started: Instant,
    deadline: Option<Instant>,
    proc: Arc<Proc>,
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicUsize>,
}

#[derive(Default)]
struct Session {
    next: u64,
    watches: Vec<Watch>,
}

/// Each conversation's watches, by its id.
static ALL: Mutex<BTreeMap<String, Session>> = Mutex::new(BTreeMap::new());

/// The deadline `input` asks for: `persistent` for none, else
/// `timeout_minutes` (1 to MAX_MINUTES, default DEFAULT_MINUTES).
pub fn minutes(input: &serde_json::Value) -> Result<Option<u64>, String> {
    if input["persistent"].as_bool() == Some(true) {
        return Ok(None);
    }
    match input.get("timeout_minutes").filter(|v| !v.is_null()) {
        None => Ok(Some(DEFAULT_MINUTES)),
        Some(v) => match v.as_u64() {
            Some(m @ 1..=MAX_MINUTES) => Ok(Some(m)),
            _ => Err(format!("`timeout_minutes` must be from 1 to {MAX_MINUTES} (or set `persistent`).")),
        },
    }
}

/// Whether `line` passes `filter` (`Spec::filter`).
fn matches(filter: &str, line: &str) -> bool {
    let line = line.to_lowercase();
    let mut alts = filter.split('|').map(|a| a.trim()).filter(|a| !a.is_empty()).peekable();
    alts.peek().is_none() || alts.any(|a| line.contains(&a.to_lowercase()))
}

/// Starts watching for conversation `conv`; returns the watch's id.
pub fn start(conv: &str, spec: Spec, deliver: Deliver) -> u64 {
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let deadline = spec.minutes.map(|m| started + Duration::from_secs(m * 60));
    let from = if spec.own { 0 } else { spec.proc.line_count() };
    let id = {
        let mut all = ALL.lock().unwrap();
        let s = all.entry(conv.to_string()).or_default();
        s.next += 1;
        s.watches.push(Watch {
            id: s.next,
            task: spec.task,
            own: spec.own,
            what: spec.what.clone(),
            filter: spec.filter.clone(),
            started,
            deadline,
            proc: spec.proc.clone(),
            stop: stop.clone(),
            sent: sent.clone(),
        });
        s.next
    };
    let conv = conv.to_string();
    std::thread::spawn(move || {
        let end = run(id, &spec, from, deadline, &stop, &sent, &deliver);
        forget(&conv, id);
        if let Some(text) = end_notice(id, &spec, end, started) {
            deliver(text);
        }
    });
    id
}

/// Why a watch ended.
#[derive(Debug, PartialEq)]
enum End {
    Stopped,
    Deadline,
    Exited,
    TooMuch,
    Gone,
}

/// The watch: waits for lines, sends them in batches, till it ends.
fn run(id: u64, spec: &Spec, mut at: usize, deadline: Option<Instant>, stop: &AtomicBool, sent: &AtomicUsize, deliver: &Deliver) -> End {
    let p = &spec.proc;
    let mut recent: VecDeque<Instant> = VecDeque::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return End::Stopped;
        }
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            if spec.own {
                p.stop();
            }
            return End::Deadline;
        }
        let wait = deadline.map_or(TICK, |d| (d - now).min(TICK));
        let (mut lines, next) = p.lines_since(at, wait);
        at = next;
        if !lines.is_empty() {
            // The lines that follow soon after go with them.
            let until = Instant::now() + BATCH;
            while p.exit().is_none() && !stop.load(Ordering::SeqCst) {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let (more, next) = p.lines_since(at, left);
                at = next;
                lines.extend(more);
            }
            let (more, next) = p.lines_since(at, Duration::ZERO);
            at = next;
            lines.extend(more);
            let lines: Vec<String> = lines.into_iter().filter(|l| matches(&spec.filter, l)).collect();
            if !lines.is_empty() {
                let now = Instant::now();
                recent.extend(std::iter::repeat_n(now, lines.len()));
                while recent.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
                    recent.pop_front();
                }
                sent.fetch_add(lines.len(), Ordering::SeqCst);
                if !deliver(notice(id, spec, &lines)) {
                    return End::Gone;
                }
                if recent.len() > MAX_PER_MINUTE {
                    if spec.own {
                        p.stop();
                    }
                    return End::TooMuch;
                }
            }
        }
        if p.exit().is_some() && at >= p.line_count() {
            return End::Exited;
        }
    }
}

/// A notification of new lines.
fn notice(id: u64, spec: &Spec, lines: &[String]) -> String {
    let matching = if spec.filter.trim().is_empty() { String::new() } else { format!(" matching `{}`", spec.filter.trim()) };
    let n = lines.len();
    let mut out = format!("Monitor {id} ({}, task {}): {n} new line{}{matching}:", spec.what, spec.task, if n == 1 { "" } else { "s" });
    let mut shown = 0;
    for l in lines.iter().take(MAX_LINES) {
        if shown > 0 && out.len() + l.len() > MAX_CHARS {
            break;
        }
        out.push('\n');
        if l.len() > MAX_CHARS {
            out.push_str(&l[..l.floor_char_boundary(MAX_CHARS)]);
            out.push_str(" [line truncated]");
        } else {
            out.push_str(l);
        }
        shown += 1;
    }
    if shown < n {
        out += &format!("\n({} more not shown; the whole output is in {})", n - shown, spec.proc.log_path().display());
    }
    out
}

/// The one notice a watch's end gets, if any: none when the model stopped
/// it or its session is gone, nor when the task it followed exited (the
/// task's own notification says so).
fn end_notice(id: u64, spec: &Spec, end: End, started: Instant) -> Option<String> {
    let ran = tasks::fmt_took(started.elapsed());
    let stopped = if spec.own { format!(", and its command (task {}) was stopped", spec.task) } else { String::new() };
    match end {
        End::Stopped | End::Gone => None,
        End::Exited if !spec.own => None,
        End::Exited => Some(format!("Monitor {id} ({}) ended: its command (task {}) {} after {ran}.", spec.what, spec.task, spec.proc.status())),
        End::Deadline => Some(format!("Monitor {id} ({}) reached its deadline after {ran} and ended{stopped}. Start it again if you still need it.", spec.what)),
        End::TooMuch => Some(format!("Monitor {id} ({}) was stopped{stopped}: over {MAX_PER_MINUTE} lines a minute is too many. Watch for fewer (a `filter`, or a command that prints only what matters).", spec.what)),
    }
}

/// Takes watch `id` out of the list (it has ended).
fn forget(conv: &str, id: u64) {
    if let Some(s) = ALL.lock().unwrap().get_mut(conv) {
        s.watches.retain(|w| w.id != id);
    }
}

/// MONITOR stop: ends watch `id` (and stops its own command), without a
/// notice.
pub fn stop(conv: &str, id: u64) -> Result<String, String> {
    let w = {
        let mut all = ALL.lock().unwrap();
        let s = all.get_mut(conv).ok_or(format!("No monitor {id}."))?;
        let i = s.watches.iter().position(|w| w.id == id).ok_or(format!("No monitor {id}."))?;
        s.watches.remove(i)
    };
    w.stop.store(true, Ordering::SeqCst);
    let how = if w.own { format!("; its command (task {}) {}", w.task, w.proc.stop()) } else { String::new() };
    Ok(format!("Monitor {id} stopped after {} ({} lines sent){how}.", tasks::fmt_took(w.started.elapsed()), w.sent.load(Ordering::SeqCst)))
}

/// Ends every watch of `conv`, without a notice, as it goes (its own
/// commands are its tasks, which go with it).
pub fn stop_all(conv: &str) {
    let watches = ALL.lock().unwrap().remove(conv).map(|s| s.watches).unwrap_or_default();
    for w in watches {
        w.stop.store(true, Ordering::SeqCst);
    }
}

/// MONITOR list.
pub fn list(conv: &str) -> String {
    let all = ALL.lock().unwrap();
    let watches = all.get(conv).map(|s| s.watches.as_slice()).unwrap_or_default();
    if watches.is_empty() {
        return "No monitors.".into();
    }
    let mut out = String::from("id\ttask\tran\tleft\tlines sent\tfilter\twhat");
    for w in watches {
        let left = w.deadline.map_or("persistent".to_string(), |d| tasks::fmt_duration(d.saturating_duration_since(Instant::now())));
        out += &format!(
            "\n{}\t{}{}\t{}\t{left}\t{}\t{}\t{}",
            w.id,
            w.task,
            if w.own { " (its own)" } else { "" },
            tasks::fmt_duration(w.started.elapsed()),
            w.sent.load(Ordering::SeqCst),
            if w.filter.is_empty() { "-" } else { w.filter.as_str() },
            w.what
        );
    }
    out
}

/// What a watch is said to watch: the summary, else the command's start.
pub fn what(summary: &str, command: &str) -> String {
    let s = crate::look::one_line(summary);
    if !s.trim().is_empty() {
        return s;
    }
    let c: String = command.head(60).replace('\n', " ");
    format!("`{}{}`", c.trim(), if c.len() < command.len() { "…" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_are_alternatives_of_text() {
        assert!(matches("", "anything"));
        assert!(matches("error|warning", "WARNING: x"));
        assert!(matches(" error ", "an error here"));
        assert!(!matches("error|warning", "all good"));
    }

    #[test]
    fn deadlines() {
        assert_eq!(minutes(&json!({})), Ok(Some(DEFAULT_MINUTES)));
        assert_eq!(minutes(&json!({"timeout_minutes": 30})), Ok(Some(30)));
        assert_eq!(minutes(&json!({"persistent": true, "timeout_minutes": 99})), Ok(None));
        assert!(minutes(&json!({"timeout_minutes": 0})).is_err() && minutes(&json!({"timeout_minutes": 31})).is_err());
    }

    /// A watch on a command of its own gets each line (filtered) as it
    /// comes, then one notice when the command exits.
    #[test]
    fn a_watch_sends_lines_as_they_come() {
        let p = tasks::spawn("echo one; sleep 1; echo ERROR two; echo three; echo error four", std::path::Path::new(".")).unwrap();
        let got = Arc::new(Mutex::new(Vec::<(Instant, String)>::new()));
        let g = got.clone();
        let deliver: Deliver = Arc::new(move |t| {
            g.lock().unwrap().push((Instant::now(), t));
            true
        });
        let t0 = Instant::now();
        let spec = Spec { task: 7, proc: p.clone(), own: true, what: "test".into(), filter: "one|error".into(), minutes: Some(1) };
        let id = start("monitor-test", spec, deliver);
        assert!(list("monitor-test").contains("\ttest"));
        while got.lock().unwrap().len() < 3 && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].1.contains("1 new line matching `one|error`:\none"), "{}", got[0].1);
        // The first came before the command ended, not with the rest.
        assert!(got[1].0.duration_since(got[0].0) > Duration::from_millis(500));
        assert!(got[1].1.contains("2 new lines") && got[1].1.contains("ERROR two\nerror four"), "{}", got[1].1);
        assert!(got[2].1.starts_with(&format!("Monitor {id} (test) ended: its command (task 7) exited with code 0")), "{}", got[2].1);
        assert_eq!(list("monitor-test"), "No monitors.");
    }

    /// Stopping a watch on its own command stops the command, with no notice.
    #[test]
    fn stopping_a_watch_stops_its_command() {
        let p = tasks::spawn("while true; do echo tick; sleep 0.2; done", std::path::Path::new(".")).unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        let deliver: Deliver = Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            true
        });
        let spec = Spec { task: 1, proc: p.clone(), own: true, what: "ticks".into(), filter: String::new(), minutes: None };
        let id = start("monitor-stop-test", spec, deliver);
        let t0 = Instant::now();
        while n.load(Ordering::SeqCst) == 0 && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(stop("monitor-stop-test", id).unwrap().starts_with(&format!("Monitor {id} stopped")));
        assert!(p.exit().is_some());
        let after = n.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(n.load(Ordering::SeqCst), after);
        assert!(stop("monitor-stop-test", id).is_err());
    }
}
