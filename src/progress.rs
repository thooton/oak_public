//! Progress meters (PROGRESS_SET): shell commands OAK runs in the working
//! folder when they are set up and then on an interval, one watcher thread
//! per meter. The last non-empty line a command prints is its value (`x/y`,
//! `N%` or a number, then optional text), shown under the goal in the
//! session box. The values are never set by hand.

use crate::tools::Head;
use crate::api;
use crate::store::Meter;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// The longest a meter's command may run; it is killed then, and the run
/// counts as failed.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// The most meters a session may have.
pub const MAX_METERS: usize = 12;
/// How often a watcher looks whether its meter is due, removed or replaced.
const POLL: Duration = Duration::from_secs(1);

/// A meter's value, as its command printed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// As shown: "46%", "1306/1306", "376".
    pub value: String,
    /// How full its bar is (0 to 1), for `x/y` and `N%`; none for a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    /// The text after the value, shown on hover.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// A number at the start of `s` ("1306", "-2.5", "1,306"), and what follows.
fn number(s: &str) -> Option<(f64, &str)> {
    let sign = s.starts_with(['-', '+']) as usize;
    let len = s[sign..].find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | ',' | '_'))).unwrap_or(s.len() - sign);
    let digits = s[sign..sign + len].trim_end_matches([',', '_', '.']);
    if !digits.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let n: f64 = digits.replace([',', '_'], "").parse().ok()?;
    let end = sign + digits.len();
    Some((if s.starts_with('-') { -n } else { n }, &s[end..]))
}

/// A meter's value from its command's output: the last non-empty line,
/// read as `x/y`, `N%` or a plain number; the text after it is the note.
pub fn parse(output: &str) -> Result<Reading, String> {
    let line = output.lines().rev().map(str::trim).find(|l| !l.is_empty()).ok_or("No output")?;
    let bad = || {
        let shown: String = line.head(80);
        format!("The last line, `{shown}`, doesn't start with x/y, N% or a number")
    };
    let (x, rest) = number(line).ok_or_else(bad)?;
    let after = rest.trim_start();
    let (fraction, rest) = if let Some(r) = after.strip_prefix('%') {
        (Some((x / 100.0).clamp(0.0, 1.0)), r)
    } else if let Some(r) = after.strip_prefix('/') {
        let (y, r) = number(r.trim_start()).ok_or_else(bad)?;
        (if y > 0.0 { Some((x / y).clamp(0.0, 1.0)) } else { None }, r)
    } else {
        (None, rest)
    };
    let value: String = line[..line.len() - rest.len()].split_whitespace().collect();
    let note = rest.trim().trim_start_matches([':', ',', ';', '-', '–', '—', '·']).trim().to_string();
    Ok(Reading { value, fraction, note })
}

/// A run without a value: why, with the exit code and the end of stderr.
fn failure(why: &str, code: i32, stderr: &str) -> String {
    let mut s = why.to_string();
    if code != 0 {
        s += &format!(" (exit code {code})");
    }
    let lines: Vec<&str> = stderr.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join("\n");
    if !tail.is_empty() {
        let tail: String = tail.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        s += "\n";
        s += &tail;
    }
    s
}

/// Numbers the stderr files of meters run at the same time.
static RUNS: AtomicUsize = AtomicUsize::new(0);

/// Runs a meter's command once in `folder` (bash, as EXECUTE does), for at
/// most `timeout`, and reads its value from stdout. The exit code doesn't
/// matter when the last line is a value (`grep -c` exits 1 on 0). While it
/// runs, `wanted` is asked now and then: false (the meter was removed or
/// replaced) kills the command, and there is no result.
pub fn run(command: &str, folder: &Path, timeout: Duration, wanted: impl Fn() -> bool) -> Option<Result<Reading, String>> {
    // Stderr goes to a file of its own, so the last line is stdout's.
    let n = RUNS.fetch_add(1, Ordering::SeqCst);
    let err = std::env::temp_dir().join(format!("oak-meter-{}-{n}.err", std::process::id()));
    let quoted = format!("'{}'", err.display().to_string().replace('\\', "/").replace('\'', r"'\''"));
    let p = match crate::tasks::spawn(&format!("{{\n{command}\n}} 2>{quoted}"), folder) {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    let t = Instant::now();
    let timed_out = loop {
        if p.exit().is_some() {
            break false;
        }
        if !wanted() {
            p.kill();
            let _ = std::fs::remove_file(&err);
            return None;
        }
        if t.elapsed() >= timeout {
            p.kill();
            break true;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();
    let _ = std::fs::remove_file(&err);
    if timed_out {
        return Some(Err(failure(&format!("Timed out after {}s", timeout.as_secs()), 0, &stderr)));
    }
    let code = p.exit().unwrap_or(-1);
    let (out, _) = p.take_new(usize::MAX);
    Some(parse(&out).map_err(|e| failure(&e, code, &stderr)))
}

/// Puts a run's result in the meter (a failed run keeps the last good
/// value) and schedules the next. True when what the meter shows changed.
pub fn record(m: &mut Meter, result: Result<Reading, String>, now: i64) -> bool {
    let before = (m.value.clone(), m.error.clone());
    match result {
        Ok(r) => {
            m.value = Some(r);
            m.error.clear();
        }
        Err(e) => m.error = e,
    }
    m.ran_at = now;
    m.next_at = now + m.every_min as i64 * 60_000;
    before != (m.value.clone(), m.error.clone())
}

/// Starts a watcher for each of the conversation's meters that has none:
/// after PROGRESS_SET, and when a session starts.
pub fn watch(conv: &Arc<crate::store::Shared>, ctx: &eframe::egui::Context) {
    let mut c = conv.lock().unwrap();
    for m in c.meters.iter_mut().filter(|m| !m.watched) {
        m.watched = true;
        let (conv, ctx, id) = (Arc::downgrade(conv), ctx.clone(), m.id);
        let _ = std::thread::Builder::new().name(format!("progress meter {id}")).spawn(move || watch_meter(conv, id, ctx));
    }
}

/// Meter `id`'s watcher: runs its command whenever it is due, one run at a
/// time, until the meter is removed or the conversation deleted. A run of
/// a command PROGRESS_SET replaced is killed and dropped.
fn watch_meter(conv: Weak<crate::store::Shared>, id: u64, ctx: eframe::egui::Context) {
    // The meter's generation, while it still has it.
    let wanted = |generation: u64| {
        conv.upgrade().is_some_and(|c| {
            let c = c.lock().unwrap();
            !c.deleted && c.meters.iter().any(|m| m.id == id && m.generation == generation)
        })
    };
    loop {
        let due = {
            let Some(arc) = conv.upgrade() else { return };
            let c = arc.lock().unwrap();
            let Some(m) = c.meters.iter().find(|m| m.id == id).filter(|_| !c.deleted) else { return };
            (m.next_at <= api::now_ms()).then(|| (m.generation, m.command.clone(), c.folder.clone()))
        };
        let Some((generation, command, folder)) = due else {
            std::thread::sleep(POLL);
            continue;
        };
        let Some(result) = run(&command, &folder, RUN_TIMEOUT, || wanted(generation)) else { continue };
        let Some(arc) = conv.upgrade() else { return };
        let mut c = arc.lock().unwrap();
        let Some(m) = c.meters.iter_mut().find(|m| m.id == id && m.generation == generation) else { continue };
        if record(m, result, api::now_ms()) {
            c.save();
        }
        drop(c);
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(s: &str) -> (String, Option<f64>, String) {
        let r = parse(s).unwrap();
        (r.value, r.fraction, r.note)
    }

    #[test]
    fn parses_the_last_line() {
        assert_eq!(read("compiling\n46%\n\n"), ("46%".into(), Some(0.46), String::new()));
        assert_eq!(read("1306/1306"), ("1306/1306".into(), Some(1.0), String::new()));
        assert_eq!(read("  12 / 48  tests left: parser, lexer\r\n"), ("12/48".into(), Some(0.25), "tests left: parser, lexer".into()));
        assert_eq!(read("376 native throws"), ("376".into(), None, "native throws".into()));
        assert_eq!(read("1,306 files"), ("1,306".into(), None, "files".into()));
        assert_eq!(read("-2.5"), ("-2.5".into(), None, String::new()));
        assert_eq!(read("12.5 % — ported"), ("12.5%".into(), Some(0.125), "ported".into()));
        // Bars stay in range; 0/0 has none.
        assert_eq!(parse("150%").unwrap().fraction, Some(1.0));
        assert_eq!(parse("7/5").unwrap().fraction, Some(1.0));
        assert_eq!(parse("0/0").unwrap().fraction, None);
        // Not a value.
        assert_eq!(parse("\n  \n"), Err("No output".into()));
        assert!(parse("ok. 3 passed").unwrap_err().contains("`ok. 3 passed`"));
        assert!(parse("3/").is_err() && parse("3/x").is_err() && parse(".").is_err() && parse("-").is_err());
        assert_eq!(failure("No output", 2, "a\n\nb\nc\nd\n"), "No output (exit code 2)\nb\nc\nd");
        assert_eq!(failure("No output", 0, ""), "No output");
    }
}
