//! Drives Oak's own Quick provider against the live service: refresh the
//! token, open the socket, send a turn, and read what comes back.
//!
//! Run with:
//!   OAK_DIR=<scratch profile> cargo test --lib quick_live -- --ignored --nocapture

use crate::api::Sse;
use crate::oak::Request;
use std::sync::mpsc::channel;

#[test]
#[ignore]
fn quick_live_turn() {
    let dir = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    let base = std::path::PathBuf::from(&dir);
    let file = [base.join("quick.json"), base.join("oak").join("quick.json")].into_iter().find(|p| p.exists());
    let file = file.unwrap_or_else(|| panic!("no quick.json in {dir}"));
    let _ = file;
    crate::api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");

    let token = crate::quick::login::access_token().expect("access token");
    println!("token: {}…", &token[..24]);
    let host = crate::quick::login::host().expect("host");
    println!("host: {host}");

    let (tx, rx) = channel();
    let folder = std::env::current_dir().unwrap().to_string_lossy().to_string();
    let body = Request {
        provider: crate::providers::Kind::Quick,
        model: "fast".into(),
        system: String::new(),
        messages: vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": std::env::var("OAK_QUICK_PROMPT").unwrap_or_else(|_| "Reply with exactly: PROBE-OK".into())}]
        })],
        folder: Some(folder),
        conv: Some(std::env::var("OAK_QUICK_THREAD").unwrap_or_else(|_| format!("oak-probe-{}", crate::api::now_ms()))),
        thinking: true,
        effort: Some("low".into()),
        ..Default::default()
    };
    let handle = std::thread::spawn(move || crate::quick::turn::stream(body, tx));

    let start = std::time::Instant::now();
    let mut text = String::new();
    let mut events = 0;
    while start.elapsed() < std::time::Duration::from_secs(240) {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Sse::Event(ev)) => {
                events += 1;
                match &ev {
                    crate::oak::Event::Text { text: t, .. } => text.push_str(t),
                    crate::oak::Event::Block { block, .. } => println!("block: {}", block["type"]),
                    crate::oak::Event::Usage(u) => println!("usage: {u:?}"),
                    crate::oak::Event::Stop { reason, .. } => println!("stop: {reason}"),
                    crate::oak::Event::Reasoning { text: t, .. } => print!("{t}"),
                    _ => {}
                }
            }
            Ok(Sse::Done) => {
                println!("\nDONE after {events} events");
                break;
            }
            Ok(Sse::Err(e)) => {
                println!("\nERR: {e}");
                break;
            }
            Ok(_) => println!("other"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                println!("… {} events so far, {text:?}", events);
            }
            Err(e) => {
                println!("channel: {e}");
                break;
            }
        }
    }
    let _ = handle.join();
    println!("\n=== TEXT ===\n{text}");
    let _ = std::fs::write(std::env::temp_dir().join("oak_quick_text.txt"), &text);
}
