//! Drives Oak's own Quick provider against the live service: refresh the
//! token, open the socket, send a turn, and read what comes back.
//!
//! Run with:
//!   OAK_DIR=<scratch profile> cargo test --lib quick_live -- --ignored --nocapture --test-threads=1
//!
//! `quick_live_turn` sends `OAK_QUICK_PROMPT`. The others are end to end
//! checks of tools Quick runs on this machine, each turn on a fresh thread:
//! `quick_live_file_type` asks for a file's byte count (the turn Quick's relay
//! used to answer with `SNCS outpost invoke error (500)`), and `quick_live_ls`
//! lists a real folder. `OAK_QUICK_REPEAT=<n>` runs each n times, since the
//! relay's failure came and went.

use crate::api::Sse;
use crate::oak::Request;
use std::sync::mpsc::channel;

/// What one live turn said: its text, and the error it ended on (if any).
fn live_turn(prompt: &str, model: &str) -> (String, Option<String>) {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    crate::api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let (tx, rx) = channel();
    let folder = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let body = Request {
        provider: crate::providers::Kind::Quick,
        model: model.into(),
        system: String::new(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": prompt}]}),
        ],
        folder: Some(folder),
        conv: Some(
            std::env::var("OAK_QUICK_THREAD")
                .unwrap_or_else(|_| format!("oak-probe-{}", crate::api::now_ms())),
        ),
        thinking: true,
        effort: Some("low".into()),
        ..Default::default()
    };
    let handle = std::thread::spawn(move || crate::quick::turn::stream(body, tx));
    let start = std::time::Instant::now();
    // Each block's text, by index: Quick's whole text replaces a block
    // (`Block`) that its deltas (`Text`) filled.
    let mut by_block: std::collections::BTreeMap<usize, String> = Default::default();
    let mut err = None;
    while start.elapsed() < std::time::Duration::from_secs(300) {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Sse::Event(ev)) => match &ev {
                crate::oak::Event::Text { index, text: t } => {
                    by_block.entry(*index).or_default().push_str(t)
                }
                crate::oak::Event::Block { index, block } => {
                    if block["type"] == "tool_call" {
                        match block.get("_oak_result") {
                            Some(r) => println!(
                                "tool: {} {} -> {}",
                                block["name"],
                                block["input"],
                                r["content"]
                                    .to_string()
                                    .chars()
                                    .take(200)
                                    .collect::<String>()
                            ),
                            None => println!("tool: {} {}", block["name"], block["input"]),
                        }
                    }
                    if block["type"] == "text" {
                        by_block.insert(*index, block["text"].as_str().unwrap_or("").to_string());
                    }
                }
                crate::oak::Event::Stop { reason, .. } => println!("stop: {reason}"),
                _ => {}
            },
            Ok(Sse::Done) => break,
            Ok(Sse::Err(e)) => {
                err = Some(e);
                break;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => {
                err = Some(format!("channel: {e}"));
                break;
            }
        }
    }
    if start.elapsed() >= std::time::Duration::from_secs(300) {
        err.get_or_insert_with(|| "the turn ran past 300s".into());
    }
    let _ = handle.join();
    let text = by_block
        .into_values()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n---\n");
    println!("=== TEXT ===\n{text}\n============");
    (text, err)
}

fn repeat() -> usize {
    std::env::var("OAK_QUICK_REPEAT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
        .max(1)
}

fn model() -> String {
    std::env::var("OAK_QUICK_MODEL").unwrap_or_else(|_| "fast".into())
}

/// A path as the user types one to Quick (`/c/Users/…`, as in the report).
fn typed(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    match s.split_once(':') {
        Some((drive, rest)) if drive.len() == 1 => format!("/{}{rest}", drive.to_lowercase()),
        _ => s,
    }
}

#[test]
#[ignore]
fn quick_live_turn() {
    let prompt =
        std::env::var("OAK_QUICK_PROMPT").unwrap_or_else(|_| "Reply with exactly: PROBE-OK".into());
    let (text, err) = live_turn(&prompt, &model());
    let _ = std::fs::write(std::env::temp_dir().join("oak_quick_text.txt"), &text);
    assert!(err.is_none(), "{err:?}");
}

/// The reported turn: Quick runs `file_type` here and answers with the byte
/// count. The answer must be the file's real size, not an apology.
#[test]
#[ignore]
fn quick_live_file_type() {
    let file = std::env::current_dir()
        .unwrap()
        .join("src")
        .join("quick")
        .join("turn.rs");
    let size = std::fs::metadata(&file).unwrap().len().to_string();
    let prompt = format!(
        "Use file_type on {} and answer with the byte count only.",
        typed(&file)
    );
    for i in 0..repeat() {
        println!("\n### run {} of {}: {prompt}", i + 1, repeat());
        let (text, err) = live_turn(&prompt, &model());
        assert!(err.is_none(), "run {}: {err:?}", i + 1);
        let digits: String = text
            .chars()
            .filter(|c| c.is_ascii_digit() || c.is_whitespace())
            .collect();
        assert!(
            digits.split_whitespace().any(|n| n == size),
            "run {}: wanted {size} bytes, Quick said: {text}",
            i + 1
        );
    }
}

/// Quick lists a real folder on this machine and names what is in it.
#[test]
#[ignore]
fn quick_live_ls() {
    let dir = std::env::current_dir().unwrap().join("src").join("quick");
    let want = ["turn.rs", "socket.rs", "outpost.rs", "mod.rs"];
    let prompt = format!(
        "List the files in {} and reply with their names only, one per line.",
        typed(&dir)
    );
    for i in 0..repeat() {
        println!("\n### run {} of {}: {prompt}", i + 1, repeat());
        let (text, err) = live_turn(&prompt, &model());
        assert!(err.is_none(), "run {}: {err:?}", i + 1);
        let missing: Vec<&str> = want.iter().copied().filter(|n| !text.contains(n)).collect();
        assert!(
            missing.is_empty(),
            "run {}: Quick's listing lacks {missing:?}: {text}",
            i + 1
        );
        assert_eq!(
            text.matches("```").count() % 2,
            0,
            "run {}: a code fence is left open (text lost): {text}",
            i + 1
        );
    }
}

/// Interrupting a running turn stops Quick at once (it is told
/// `stop_generation`), the stream ends promptly, and the thread still takes
/// the next message afterwards (issue 3: an interrupted turn used to leave
/// Quick running on the thread and break it).
#[test]
#[ignore]
fn quick_live_interrupt_then_carry_on() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    crate::api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let folder = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let thread = format!("oak-interrupt-{}", crate::api::now_ms());
    let body = |prompt: &str| Request {
        provider: crate::providers::Kind::Quick,
        model: model(),
        system: String::new(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": prompt}]}),
        ],
        folder: Some(folder.clone()),
        conv: Some(thread.clone()),
        thinking: true,
        effort: Some("low".into()),
        ..Default::default()
    };
    let (tx, rx) = channel();
    let long = body(
        "Run this Python with run_python_outpost_local: `import time; time.sleep(120)`. Then say DONE.",
    );
    let t = thread.clone();
    let started = std::time::Instant::now();
    let handle = std::thread::spawn(move || crate::quick::turn::stream(long, tx));
    // Let the turn begin and reach its tool call.
    std::thread::sleep(std::time::Duration::from_secs(8));
    let abort_at = std::time::Instant::now();
    crate::quick::turn::abort(&t, crate::quick::turn::StopKind::Interrupt);
    let mut text = String::new();
    let mut ended = None;
    while started.elapsed() < std::time::Duration::from_secs(60) {
        match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Sse::Event(ev)) => {
                if let crate::oak::Event::Text { text: d, .. } = &ev {
                    text.push_str(d);
                }
            }
            Ok(Sse::Done) => {
                ended = Some(());
                break;
            }
            Ok(Sse::Err(e)) => panic!("the interrupted turn failed: {e}"),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                ended = Some(());
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let took = abort_at.elapsed();
    handle.join().ok();
    assert!(ended.is_some(), "the interrupted turn never ended");
    assert!(
        took < std::time::Duration::from_secs(15),
        "the interrupt took {took:?}: {text}"
    );
    println!("interrupted after {took:?}; text so far: {text}");
    // The same thread takes the next message.
    let (text2, err) = live_on(&thread, "Reply with exactly: AFTER-OK", &folder);
    assert!(err.is_none(), "{err:?}");
    assert!(
        text2.contains("AFTER-OK"),
        "the thread broke after the interrupt: {text2}"
    );
}

/// A pause asked for mid-turn takes hold at the turn's next step (issue 4:
/// it used to wait for the whole Quick turn), Quick is told to stop with
/// `explicit_pause`, and the thread takes the next message.
#[test]
#[ignore]
fn quick_live_pause_takes_hold_at_the_next_step() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    crate::api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let folder = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let thread = format!("oak-pause-{}", crate::api::now_ms());
    let body = Request {
        provider: crate::providers::Kind::Quick,
        model: model(),
        system: String::new(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "Run this Python with run_python_outpost_local: `import time; time.sleep(15)`. Then say DONE."}]}),
        ],
        folder: Some(folder.clone()),
        conv: Some(thread.clone()),
        thinking: true,
        effort: Some("low".into()),
        ..Default::default()
    };
    let (tx, rx) = channel();
    let t = thread.clone();
    let started = std::time::Instant::now();
    let handle = std::thread::spawn(move || crate::quick::turn::stream(body, tx));
    std::thread::sleep(std::time::Duration::from_secs(6));
    let asked_at = std::time::Instant::now();
    crate::quick::turn::set_pause(&t, true);
    let mut ended = false;
    while started.elapsed() < std::time::Duration::from_secs(90) {
        match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Sse::Done) => {
                ended = true;
                break;
            }
            Ok(Sse::Err(e)) => panic!("the paused turn failed: {e}"),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                ended = true;
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let took = asked_at.elapsed();
    handle.join().ok();
    crate::quick::turn::set_pause(&thread, false);
    assert!(ended, "the pause never took hold");
    assert!(
        took < std::time::Duration::from_secs(20),
        "the pause took {took:?}"
    );
    let (text, err) = live_on(&thread, "Reply with exactly: RESUMED-OK", &folder);
    assert!(err.is_none(), "{err:?}");
    assert!(
        text.contains("RESUMED-OK"),
        "the thread broke after the pause: {text}"
    );
}

/// A live turn on an existing `thread`, in `folder`.
fn live_on(thread: &str, prompt: &str, folder: &str) -> (String, Option<String>) {
    let body = Request {
        provider: crate::providers::Kind::Quick,
        model: model(),
        system: String::new(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": prompt}]}),
        ],
        folder: Some(folder.to_string()),
        conv: Some(thread.to_string()),
        thinking: true,
        effort: Some("low".into()),
        ..Default::default()
    };
    let (tx, rx) = channel();
    let handle = std::thread::spawn(move || crate::quick::turn::stream(body, tx));
    let start = std::time::Instant::now();
    let mut by_block: std::collections::BTreeMap<usize, String> = Default::default();
    let mut err = None;
    while start.elapsed() < std::time::Duration::from_secs(120) {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Sse::Event(ev)) => match &ev {
                crate::oak::Event::Text { index, text } => {
                    by_block.entry(*index).or_default().push_str(text)
                }
                crate::oak::Event::Block { index, block } if block["type"] == "text" => {
                    by_block.insert(*index, block["text"].as_str().unwrap_or("").to_string());
                }
                _ => {}
            },
            Ok(Sse::Done) => break,
            Ok(Sse::Err(e)) => {
                err = Some(e);
                break;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    handle.join().ok();
    (
        by_block.values().cloned().collect::<Vec<_>>().join("\n"),
        err,
    )
}
