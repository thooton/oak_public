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
    let thread = std::env::var("OAK_QUICK_THREAD").unwrap_or_else(|_| format!("oak-probe-{}", crate::api::now_ms()));
    live_turn_on(prompt, model, &thread)
}

/// `live_turn`, on thread `thread`.
fn live_turn_on(prompt: &str, model: &str, thread: &str) -> (String, Option<String>) {
    live_turn_in(prompt, model, thread, None)
}

/// `live_turn_on`, in Quick session `session` (its thread, an edit).
fn live_turn_in(
    prompt: &str,
    model: &str,
    thread: &str,
    session: Option<crate::quick::recovery::Session>,
) -> (String, Option<String>) {
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
        conv: Some(thread.to_string()),
        thinking: true,
        effort: Some("low".into()),
        quick_session: session,
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
                crate::oak::Event::Stop { reason, detail, .. } => println!("stop: {reason} detail: {detail:?}"),
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
    // (`OAK_QUICK_PROMPT_FILE`: a prompt too big for the environment, such
    // as a conversation's handoff message.)
    let prompt = match std::env::var("OAK_QUICK_PROMPT_FILE") {
        Ok(f) => std::fs::read_to_string(f).expect("OAK_QUICK_PROMPT_FILE"),
        Err(_) => std::env::var("OAK_QUICK_PROMPT").unwrap_or_else(|_| "Reply with exactly: PROBE-OK".into()),
    };
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

/// A message of `OAK_QUICK_BIG` characters (default 450000) on a fresh
/// thread: whether Quick takes a message that size.
#[test]
#[ignore]
fn quick_live_big_message() {
    let n: usize = std::env::var("OAK_QUICK_BIG").ok().and_then(|s| s.parse().ok()).unwrap_or(450_000);
    let mut filler = String::new();
    let mut i = 0;
    while filler.len() < n {
        filler.push_str(&format!("Line {i}: the quick brown fox jumps over the lazy dog.\n"));
        i += 1;
    }
    filler.truncate(n);
    let prompt = format!("{filler}\n\nIgnore the filler above. Reply with exactly: BIG-OK");
    println!("message: {} bytes", prompt.len());
    let (text, err) = live_turn(&prompt, &model());
    println!("err: {err:?}");
    assert!(err.is_none(), "{err:?}");
    assert!(text.contains("BIG-OK"), "{text}");
}

/// A file uploaded to a fresh thread's workspace before the thread's first
/// message: whether Quick has it there, and how much of it one read brings
/// back (`OAK_QUICK_UPLOAD_LINES` lines, default 6000, about 335k characters).
#[test]
#[ignore]
fn quick_live_upload_before_first_message() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    crate::api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let n: usize = std::env::var("OAK_QUICK_UPLOAD_LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(6000);
    let mut body = String::new();
    for i in 0..n {
        body.push_str(&format!("Line {i}: the quick brown fox jumps over the lazy dog.\n"));
    }
    body.push_str("END-MARKER-XYZ\n");
    let thread = format!("oak-upload-{}", crate::api::now_ms());
    // The upload goes through the login the thread's turn will (`stream`).
    crate::quick::login::set_using(crate::quick::login_for(&thread).map(|(a, _)| a));
    let up = crate::quick::outpost::upload(body.as_bytes(), "quick-conversation-probe.md", "text/plain", &thread);
    println!("upload: {up:?}");
    let up = up.expect("the upload");
    let prompt = format!(
        "A file is in your workspace at {} ({}). Read the whole of it with as few tool calls as you can (one if possible), then reply with: the number of tool calls you used, the highest line number you saw, and whether you saw END-MARKER-XYZ.",
        up["path"].as_str().unwrap_or(""),
        up["workspace_path"].as_str().unwrap_or("")
    );
    let (text, err) = live_turn_on(&prompt, &model(), &thread);
    println!("err: {err:?}");
    assert!(err.is_none(), "{err:?}");
    assert!(!text.is_empty());
}

/// The conversation in `OAK_QUICK_CONV` (a conversation file) moved to a
/// fresh Quick thread, as a switch sends it: its whole transcript unseen.
#[test]
#[ignore]
fn quick_live_switch_conversation() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    crate::api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let path = std::env::var("OAK_QUICK_CONV").expect("OAK_QUICK_CONV");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut messages = v["messages"].as_array().cloned().unwrap_or_default();
    let ask = std::env::var("OAK_QUICK_ASK").unwrap_or_else(|_| "Reply with exactly: SWITCH-OK".into());
    messages.push(serde_json::json!({"role": "user", "content": [{"type": "text", "text": ask}]}));
    let unseen = crate::quick::all_unseen(&messages);
    let thread = format!("oak-switch-{}", crate::api::now_ms());
    // A login kept here, not on the vault (a test has no plugins).
    if let Some(local) = crate::quick::login::all().into_iter().find(|l| !l.remote) {
        crate::rotation::used(crate::api::Provider::Quick, &thread, &local.id, 0, crate::api::now_ms());
    }
    let scratch = std::env::current_dir().unwrap().join(".oak").join("tmp").join("temp157");
    let body = Request {
        provider: crate::providers::Kind::Quick,
        model: model(),
        system: String::new(),
        messages,
        unseen,
        folder: Some(std::env::current_dir().unwrap().to_string_lossy().to_string()),
        conv: Some(thread.clone()),
        thinking: true,
        effort: Some(std::env::var("OAK_QUICK_EFFORT").unwrap_or_else(|_| "low".into())),
        scratch: Some(scratch.to_string_lossy().to_string()),
        ..Default::default()
    };
    // (The message itself isn't built here: one too long to send goes in
    // files, `handoff`, which only the turn's own login should upload.)
    let (tx, rx) = channel();
    let handle = std::thread::spawn(move || crate::quick::turn::stream(body, tx));
    let start = std::time::Instant::now();
    let mut text = String::new();
    let mut err = None;
    while start.elapsed() < std::time::Duration::from_secs(300) {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Sse::Event(crate::oak::Event::Text { text: t, .. })) => text.push_str(&t),
            Ok(Sse::Event(crate::oak::Event::Block { block, .. })) if block["type"] == "tool_call" => {
                println!("tool: {} {}", block["name"], block["input"]);
            }
            Ok(Sse::Done) => break,
            Ok(Sse::Err(e)) => {
                err = Some(e);
                break;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    handle.join().ok();
    println!("text: {text}\nerr: {err:?}");
    assert!(err.is_none(), "{err:?}");
}

/// A revert goes on as Quick's own edit of the message (`edit_message`):
/// Quick's thread forgets what the edited message said and all after it,
/// and answers the new text in its place, on the same thread.
#[test]
#[ignore]
fn quick_live_edit_message() {
    let thread = format!("oak-edit-{}", crate::api::now_ms());
    let first = "Remember this code word: APPLE. Reply with just OK.";
    let (_, err) = live_turn_on(first, &model(), &thread);
    assert!(err.is_none(), "{err:?}");
    let sent = crate::quick::turn::take_sent(&thread);
    let original = sent.first().expect("the first message, as sent").clone();
    assert!(!original.edit);
    let session = crate::quick::recovery::Session {
        account: original.account.clone(),
        thread: thread.clone(),
        edit: Some(original.id.clone()),
        ..Default::default()
    };
    let edited = "Remember this code word: BANANA. Reply with just OK.";
    let (_, err) = live_turn_in(edited, &model(), &thread, Some(session.clone()));
    assert!(err.is_none(), "the edit: {err:?}");
    let sent = crate::quick::turn::take_sent(&thread);
    assert!(sent.first().is_some_and(|s| s.edit && s.thread == thread), "{sent:?}");
    let on = crate::quick::recovery::Session { edit: None, ..session };
    let ask = "What code word did I give you, and did I ever give you any other? Reply with the word(s) only.";
    let (text, err) = live_turn_in(ask, &model(), &thread, Some(on));
    assert!(err.is_none(), "{err:?}");
    let t = text.to_uppercase();
    assert!(t.contains("BANANA") && !t.contains("APPLE"), "the thread still has the edited message: {text}");
}
