//! A message longer than Quick's service takes (`MESSAGE_MAX`): the whole of
//! it goes to the thread's workspace in files, each small enough for one of
//! Quick's reads, and the message says where they are and to read them all at
//! once (`deliver`). A copy of each stays in Oak's folder (`dir`). Quick's
//! reads of them are its catching up (`quick::is_boot`): no model is sent them
//! again, and the transcript shows each as one closed row. Until Quick reads
//! one, the first of Oak's calls it makes is answered with a reminder instead
//! of being run (`remind`), once.

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

/// The longest message Oak sends Quick (bytes). Quick's service turns away a
/// message past about 49,500 (measured 26 Nov 2026: 49,053 went, 49,953 was
/// refused), with an error that says nothing of why.
pub(crate) const MESSAGE_MAX: usize = 45_000;

/// How much of a part one read of Quick's must carry (`room`): Quick returns
/// about 116,000 characters of a file once its result is serialized, which
/// takes two for a quote, a backslash or a line break and six for a
/// character past ASCII.
const PART_ROOM: usize = 100_000;

/// How long the copies in Oak's folder are kept.
const KEEP_FOR: Duration = Duration::from_secs(3 * 24 * 3600);

/// Where the copies go: `quick-handoff` in Oak's own folder.
pub(crate) fn dir() -> std::path::PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join(format!("oak-quick-handoff-{}", std::process::id()));
    }
    crate::store::dir().join("quick-handoff")
}

/// What `c` takes of a serialized result (`PART_ROOM`).
fn room(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c if c.is_ascii() => 1,
        c if (c as u32) > 0xFFFF => 12,
        _ => 6,
    }
}

/// `text` in parts of at most `max` of `room`, at line ends where it can be.
pub(crate) fn split(text: &str, max: usize) -> Vec<String> {
    let mut parts = vec![];
    let mut cur = String::new();
    let mut used = 0;
    for line in text.split_inclusive('\n') {
        let cost: usize = line.chars().map(room).sum();
        if used + cost > max && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
            used = 0;
        }
        if cost <= max {
            cur.push_str(line);
            used += cost;
            continue;
        }
        // A line longer than a part goes over several.
        for c in line.chars() {
            if used + room(c) > max && !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
                used = 0;
            }
            cur.push(c);
            used += room(c);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// A thread sent its message in files that Quick hasn't read yet.
struct Pending {
    names: Vec<String>,
    reminder: String,
    reminded: bool,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

/// One of Quick's calls on `thread` (a `tool_start`, `tool_arguments`,
/// `tool_call` or `tool_result` frame): one that names a file of its message
/// is Quick reading them, and nothing more is waited for.
pub(crate) fn saw(thread: &str, frame: &Value) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = pending.get(thread) else { return };
    let said = frame.to_string();
    if p.names.iter().any(|n| said.contains(n.as_str())) {
        pending.remove(thread);
    }
}

/// What Oak answers the first of its own calls Quick makes on `thread` before
/// it has read the files of its message, instead of running it: None when
/// there are none, they were read, or it was reminded once already.
pub(crate) fn remind(thread: &str) -> Option<String> {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let p = pending.get_mut(thread)?;
    if p.reminded {
        return None;
    }
    p.reminded = true;
    Some(p.reminder.clone())
}

/// Takes out the copies older than `KEEP_FOR`.
fn sweep(d: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(d) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > KEEP_FOR);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `text`, a message for `thread` too long to send (`MESSAGE_MAX`), put in
/// files in the thread's workspace (and Oak's folder): the message to send
/// instead, which says where they are and how to read them.
pub(crate) fn deliver(text: &str, thread: &str) -> String {
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    sweep(&d);
    let safe: String = thread.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    // (Its own names each time: a second message on the thread gets files
    // of its own, not the first's renamed by Quick.)
    let seq = api::now_ms().rem_euclid(1_000_000);
    let parts = split(text, PART_ROOM);
    let names: Vec<String> = (1..=parts.len()).map(|k| format!("{SO_FAR_FILE}{safe}-{seq}-{k}.md")).collect();
    let local: Vec<std::path::PathBuf> = names.iter().map(|n| d.join(n)).collect();
    for (p, body) in local.iter().zip(&parts) {
        let _ = std::fs::write(p, body);
    }
    // All at once: each waits on Quick's scan of what it is given.
    let using = login::using();
    let uploaded: Vec<Result<Value, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = names
            .iter()
            .zip(&parts)
            .map(|(name, body)| {
                let using = using.clone();
                s.spawn(move || {
                    login::set_using(using);
                    crate::quick::outpost::upload(body.as_bytes(), name, "text/plain", thread)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("the upload stopped".into())))
            .collect()
    });
    let place = |k: usize| match &uploaded[k] {
        Ok(v) => v["workspace_path"].as_str().unwrap_or(&names[k]).to_string(),
        Err(_) => format!("{} (on this device)", local[k].display().to_string().replace('\\', "/")),
    };
    let list: Vec<String> = (0..parts.len()).map(|k| format!("{}. {}", k + 1, place(k))).collect();
    let list = list.join("\n");
    let failed: Vec<&String> = uploaded.iter().filter_map(|u| u.as_ref().err()).collect();
    let where_ = if failed.is_empty() {
        "in your workspace".to_string()
    } else {
        format!(
            "in your workspace, except those marked as on this device, which Oak couldn't put there ({}); read those on this device with file_read, under /outposts/<this device>/",
            failed[0]
        )
    };
    let n = parts.len();
    let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
    let message = format!(
        "[Oak] What Oak has for you this time is longer than one Quick message can carry, so Oak put all of it, word for word, in {files} {where_} ({} characters in all). Before anything else, read every one of them in full with file_read, all at once in one step: one call per file, each with max_chars=116000 and max_lines=100000, and if a read gives a next_offset, read on from there. Read them yourself rather than scanning them with code: they hold your instructions and the context you are working in, and you need all of it. Then go on as if their text were this message (it starts with the instructions and tools Oak has loaded for you, then the conversation so far, then the newest message), carrying on from where the conversation leaves off, without starting over or mentioning the files.\n\nThe files, in order:\n{list}",
        text.chars().count()
    );
    let reminder = format!(
        "Oak didn't run this call: first read the files that hold this conversation's context and your instructions, all at once with one file_read each (max_chars=116000, max_lines=100000), then make the call again. The files, in order:\n{list}"
    );
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(
        thread.to_string(),
        Pending {
            names,
            reminder,
            reminded: false,
        },
    );
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_text_splits_at_line_ends_within_a_reads_room() {
        let text: String = (0..20_000).map(|i| format!("Line {i}: \"quoted\" and caf\u{e9}\n")).collect();
        let parts = split(&text, PART_ROOM);
        assert!(parts.len() > 1);
        assert_eq!(parts.concat(), text, "nothing is lost or reordered");
        for p in &parts {
            assert!(p.chars().map(room).sum::<usize>() <= PART_ROOM);
            assert!(p.ends_with('\n'), "a part ends at a line end");
        }
    }

    #[test]
    fn a_line_longer_than_a_part_goes_over_several() {
        let text = "x".repeat(PART_ROOM * 2 + 5);
        let parts = split(&text, PART_ROOM);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn the_reminder_comes_once_and_not_after_a_read() {
        let pending = |t: &str| {
            PENDING.lock().unwrap().insert(
                t.into(),
                Pending {
                    names: vec!["quick-conversation-th1-5-1.md".into()],
                    reminder: "read first".into(),
                    reminded: false,
                },
            );
        };
        pending("th-remind");
        assert_eq!(remind("th-remind").as_deref(), Some("read first"));
        assert_eq!(remind("th-remind"), None, "once");
        pending("th-read");
        saw("th-read", &json!({"type": "tool_start", "payload": {"tool_name": "file_read", "arguments": {"path": "workspace/attached_files/quick-conversation-th1-5-1.md"}}}));
        assert_eq!(remind("th-read"), None, "read already");
        assert_eq!(remind("th-none"), None);
    }
}
