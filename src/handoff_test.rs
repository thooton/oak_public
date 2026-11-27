//! One run of classifier avoidance in a whole Oak: a conversation on Claude
//! whose reply a safety classifier declines, the plugin loading, the
//! switch to DeepSeek, the note, and the new conversation that carries on.
//! Prints what happened; it is run by hand against `bench/tools/fake_refusal.py`
//! (which refuses the first reply) and the fake OpenAI-compatible Refuseless
//! (`fake_deepseek.py`).
//!
//!     OAK_API_BASE=http://127.0.0.1:5799 REFUSELESS_BASE_URL=http://127.0.0.1:5800 \
//!     OAK_PLUGINS=/home/sam/Documents/oak_private/target/debug \
//!     cargo test --release handoff_in_a_whole_app -- --ignored --nocapture -p oak
//!
//! It is in the tests this repository keeps for such a run (it needs the
//! private repository's plugin and fakes).

#![cfg(test)]

use crate::app_hover_test::Headless;
use crate::*;

/// A conversation whose first reply is refused, waiting for one.
fn refused_conversation() -> serde_json::Value {
    let now = api::now_ms();
    serde_json::json!({
        "id": "1700000000001",
        "title": "Cyber work",
        "folder": crate::temp_dir().join("oak-handoff-folder"),
        "provider": "claude",
        "model": "claude-opus-5-5",
        "effort": "high",
        "role": "",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Write the exploit POC."}]}],
        "last_message_at": now,
        "next_task_id": 0,
    })
}

fn frames(n: usize, h: &mut Headless, app: &mut App) {
    for _ in 0..n {
        h.events(app, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}

#[test]
#[ignore]
fn handoff_in_a_whole_app() {
    let folder = crate::temp_dir().join("oak-handoff-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let (dir, mut h, mut app) = Headless::start("handoff", &[refused_conversation()]);
    // The plugins in `OAK_PLUGINS` (the private repository's), this one
    // among them.
    let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built plugins");
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
    unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
    crate::plugins::load();
    let trouble: Vec<String> = crate::plugins::found().iter().filter_map(|f| f.trouble.clone()).collect();
    assert!(trouble.is_empty(), "a plugin didn't load: {trouble:?}");
    assert!(crate::plugins::parts().hooks.iter().any(|h| h.has_composer_corner()), "the plugin isn't loaded");
    // The mode on: as a click on the row's switch leaves it.
    std::fs::write(dir.join("classifier-avoidance.json"), r#"{"on":true}"#).unwrap();
    // A Refuseless key: `REFUSELESS_API_KEY`, which `load_auth` (run by
    // `App::new`) reads as it starts.
    assert!(crate::refuseless::on(), "the Refuseless key (REFUSELESS_API_KEY) wasn't read");

    // The conversation opens and its reply is refused; the plugin takes it
    // from there.
    app.open("1700000000001".to_string());
    let mut state = serde_json::Value::Null;
    for _ in 0..120 {
        frames(1, &mut h, &mut app);
        if let Ok(bytes) = std::fs::read(dir.join("classifier-avoidance.json")) {
            state = serde_json::from_slice(&bytes).unwrap_or_default();
        }
        // The stored conversation ends in an unanswered message, and
        // nothing opens it, so the test resumes it as Oak would (a
        // conversation refused while it runs is busy already).
        if let Some(s) = app.plugin_session("1700000000001")
            && !s.busy
            && state["jobs"].as_array().is_none_or(|j| j.is_empty())
        {
            let _ = app.plugin_resume("1700000000001");
        }
        let jobs = state["jobs"].as_array().cloned().unwrap_or_default();
        let Some(job) = jobs.first() else { continue };
        if job["step"] == "Done" || job["error"].as_str().is_some_and(|e| !e.is_empty()) {
            break;
        }
    }
    crate::plugins::exiting();
    let _ = std::fs::remove_dir_all(dir);

    let jobs = state["jobs"].as_array().cloned().unwrap_or_default();
    let job = jobs.first().expect("the plugin started a handoff");
    assert!(job["error"].is_null() || job["error"].as_str() == Some(""), "the handoff failed: {job}");
    assert_eq!(job["step"], "Done", "the handoff didn't finish: {job}");
    let made = job["made"].as_str().unwrap_or("");
    assert!(!made.is_empty(), "no new conversation was made");
    let new = app.plugin_session(made).expect("the new conversation is there");
    assert!(app.sessions.contains_key(made), "the new conversation isn't open");

    // The new conversation: the same folder, role and model that was
    // refused, its first message the note and the instruction to carry on.
    assert_eq!(new.folder, folder.display().to_string());
    assert_eq!((new.provider.as_str(), new.model.as_str()), ("claude", "claude-opus-5-5"));
    let c = app.sessions.get(made).unwrap().conv.lock().unwrap();
    assert!(c.title.contains("Cyber"), "the title is carried over: {}", c.title);
    let first: String = c.messages.iter().flat_map(|m| m["content"].as_array().into_iter().flatten()).filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    assert!(first.contains("Done so far: the POC's parser"), "the note is in the new conversation: {first}");
    assert!(first.contains("Carry on"), "it is told to carry on: {first}");
    assert!(first.contains("category cyber"), "the refusal is in it too: {first}");
    drop(c);

    // The refused conversation kept its model, its own newest turn is
    // DeepSeek's note, and the refusal isn't handed off a second time.
    let refused = app.plugin_session("1700000000001").unwrap();
    assert_eq!((refused.provider.as_str(), refused.model.as_str()), ("claude", "claude-opus-5-5"));
    assert!(refused.reply.as_deref().unwrap_or("").contains("Done so far"), "the note is the refused conversation's last reply");
    assert_eq!(state["done"].as_array().map(|d| d.len()), Some(1));
    assert_eq!(state["jobs"].as_array().map(|j| j.len()), Some(1));
}
