//! Opt-in check of what smart context's summary requests send, against a
//! fake API that records every request: each carries only the part of the
//! conversation it replaces, as the model saw it.

use super::*;

/// The requests a fake API (`bench/tools/fake_summary_capture.py` in the
/// soupskin-b2/oak repository) logged, oldest first.
fn captured(log: &str) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Every `SENT-<WORD>-<n>` sentinel in `v`, once each, sorted.
fn sentinels(v: &Value) -> Vec<String> {
    let s = v.to_string();
    let mut out: Vec<String> = vec![];
    for (i, _) in s.match_indices("SENT-") {
        let t: String = s[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let t = t.trim_end_matches('-').to_string();
        if t.matches('-').count() == 2 && !out.contains(&t) {
            out.push(t);
        }
    }
    out.sort();
    out
}

/// What the model sees of `messages` (sentinels): not another provider's
/// reasoning, Oak's own keys or the output of the calls in `cleared`.
fn visible(messages: &[Value], cleared: &[String]) -> Vec<String> {
    let mut seen = crate::oak::clean(messages.to_vec());
    for m in &mut seen {
        if let Some(content) = m["content"].as_array_mut() {
            content.retain(|b| !(crate::oak::is_reasoning(b) && b["by"] != "claude"));
            content.retain(|b| !(b["type"] == "tool_result" && b["call_id"].as_str().is_some_and(|id| cleared.iter().any(|c| c == id))));
        }
    }
    sentinels(&json!(seen))
}

/// The summary state a summary request is built on.
struct Before {
    cut: usize,
    text: String,
    cleared: Vec<String>,
}

fn before(s: &Session) -> Before {
    let c = s.conv.lock().unwrap();
    let (cut, text) = c.rolling.summary.as_ref().map_or((0, String::new()), |s| (s.cut, s.text.clone()));
    Before { cut, text, cleared: c.rolling.cleared_calls(&c.messages).into_iter().map(String::from).collect() }
}

/// Smart context's summary requests, as a fake API recorded them, with
/// sentinel-marked messages: background summaries (three in a row, the
/// third after a save and reload, with tool output a summary cleared and a
/// reverted exchange in between) and the blocking summary taken when a
/// request is refused as too long. Each request carries exactly the earlier
/// summary and what the model saw of the messages it replaces: none of the
/// part kept verbatim, of cleared tool output, another provider's
/// reasoning, Oak's own keys or what was reverted.
///
/// ```text
/// python3 bench/tools/fake_summary_capture.py --port 5702 --log /tmp/cap.jsonl --overflow SENT-BLOCK-99 &
/// OAK_DIR=$(mktemp -d) OAK_API_BASE=http://127.0.0.1:5702 OAK_CAPTURE_LOG=/tmp/cap.jsonl \
///   cargo test --lib rolling_capture -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn rolling_capture() {
    let base = std::env::var("OAK_API_BASE").expect("OAK_API_BASE: the fake API's address");
    assert!(base.starts_with("http://127.0.0.1") || base.starts_with("http://localhost"), "only against a local fake API");
    let log = std::env::var("OAK_CAPTURE_LOG").expect("OAK_CAPTURE_LOG: the fake's log");
    let dir = std::env::var_os("OAK_DIR").expect("OAK_DIR: a scratch folder (it gets a throwaway account)");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        std::path::Path::new(&dir).join("accounts.json"),
        json!({"list": [{"id": 1, "auth": {"type": "api_key", "key": "sk-ant-capture"}}], "current": 1, "next_id": 2}).to_string(),
    )
    .unwrap();
    assert!(api::load_auth());
    // Keeping more than `rolling::recent()`: a summary clears the older tool
    // output of the part it keeps, which the next one then replaces.
    rolling::set_config(rolling::Config { trigger: 60_000, keep: 30_000 });
    let filler = |tag: &str, lines: usize| -> String {
        let lines: String = (0..lines).map(|i| format!("{tag} log line {i}: compiled crate number {i} without warnings.\n")).collect();
        format!("{tag}\n{lines}")
    };
    // About 3k tokens: a marked prompt, a tool call and its marked output,
    // and a marked answer (with, in some, what the model never sees: another
    // provider's reasoning and Oak's own keys).
    let turn = |i: usize, hidden: bool| {
        let id = format!("call-{i:02}");
        let mut answer = json!({"role": "assistant", "content": [{"type": "text", "text": format!("SENT-NOTED-{i:02}")}]});
        if hidden {
            answer["content"].as_array_mut().unwrap().insert(0, json!({"type": "reasoning", "by": "elsewhere", "text": format!("SENT-HIDDEN-{i:02}")}));
            answer["content"][1]["_oak_note"] = json!(format!("SENT-HIDDEN-{}", i + 50));
        }
        vec![
            json!({"role": "user", "content": [{"type": "text", "text": filler(&format!("SENT-TURN-{i:02}"), 60)}]}),
            json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": "EXECUTE", "input": {"command": format!("echo SENT-CALL-{i:02}"), "summary": "Run it", "timeout": 5}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": filler(&format!("SENT-TOOL-{i:02}"), 100)}]}),
            answer,
        ]
    };
    let conv = |name: &str, messages: Vec<Value>| -> Conversation {
        let folder = crate::temp_dir().join(format!("oak-rolling-capture-{name}"));
        std::fs::create_dir_all(&folder).unwrap();
        Conversation {
            id: format!("rolling-capture-{name}-{}", api::now_ms()),
            title: "rolling capture".into(),
            folder,
            model: "claude-opus-4-5".into(),
            effort: "low".into(),
            messages: messages.into(),
            ..Default::default()
        }
    };
    let wait = |s: &Session, what: &str| {
        let t = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let l = s.live.lock().unwrap();
            if !l.busy && !l.compacting && l.queued.is_empty() {
                assert!(l.error.is_none(), "{what}: {:?}", l.error);
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(120), "{what} timed out");
        }
    };
    let summaries = || captured(&log).into_iter().filter(|r| r["kind"] == "summary").collect::<Vec<_>>();
    // A summary request (made on `b`): what it carries is the earlier
    // summary's sentinels and what the model saw of the messages it
    // replaces, nothing else; then the instruction, last.
    let check = |what: &str, s: &Session, b: &Before, r: &Value| {
        let c = s.conv.lock().unwrap();
        let cut = c.rolling.summary.as_ref().expect("the summary installed").cut;
        let msgs = r["body"]["messages"].as_array().unwrap();
        let carried = sentinels(&r["body"]);
        let mut expect = sentinels(&json!(b.text));
        expect.extend(visible(&c.messages[b.cut..cut], &b.cleared));
        expect.sort();
        expect.dedup();
        let kept = sentinels(&json!(c.messages[cut..]));
        println!("== {what}: messages {}..{cut} replaced, {cut}..{} kept", b.cut, c.messages.len());
        println!("   carried: {carried:?}");
        println!("   kept verbatim (none carried): {kept:?}");
        assert_eq!(carried, expect, "{what}: the earlier summary and what the model saw before the cut");
        assert!(!carried.iter().any(|t| kept.contains(t)), "{what}: nothing kept verbatim");
        assert!(!carried.iter().any(|t| t.contains("HIDDEN") || t.contains("REVERT")), "{what}: nothing the model never saw");
        let last = msgs.last().unwrap()["content"].as_array().unwrap().last().unwrap().clone();
        assert!(last["text"].as_str().is_some_and(rolling::is_housekeeping), "{what}: the instruction ends it");
        if !b.text.is_empty() {
            let first = msgs[0].to_string();
            assert!(first.contains("[Summary of the earlier part of this conversation]") && first.contains(&b.text[..9]), "{what}: it starts with the earlier summary");
        }
        // The tool output the earlier summary cleared that this one replaces
        // goes as cleared.
        let gone: Vec<String> =
            sentinels(&json!(c.messages[b.cut..cut])).into_iter().filter(|t| t.starts_with("SENT-TOOL") && !expect.contains(t)).collect();
        println!("   tool output cleared before, not carried: {gone:?}");
        gone
    };
    // Turns of the conversation and a short message, until the background
    // summary number `k` is made.
    let grow = |s: &Session, from: usize, k: usize| -> usize {
        for i in from..from + 30 {
            {
                let mut c = s.conv.lock().unwrap();
                for m in turn(i, i % 5 == 0) {
                    c.messages.push(m);
                }
            }
            s.send(format!("SENT-ASK-{i:02} Go on."), vec![]);
            wait(s, "a turn");
            if summaries().len() >= k {
                return i + 1;
            }
        }
        panic!("no summary {k}");
    };

    // 1. The first background summary.
    let prior: Vec<Value> = (0..20).flat_map(|i| turn(i, i % 5 == 3)).collect();
    let s = Session::start(conv("background", prior), eframe::egui::Context::default());
    let b = before(&s);
    let next = grow(&s, 20, 1);
    let r = summaries();
    assert_eq!(r.len(), 1, "one summary request");
    check("background summary 1", &s, &b, &r[0]);

    // An exchange reverted: never sent again.
    let b = before(&s);
    assert!(b.cleared.len() > 1, "the summary cleared older tool output it kept: {:?}", b.cleared);
    let back_to = s.conv.lock().unwrap().messages.len();
    s.send("SENT-REVERT-01 Never mind this.".into(), vec![]);
    wait(&s, "the exchange to revert");
    s.rewind(RevertTo::End(back_to));
    let t = Instant::now();
    while s.conv.lock().unwrap().messages.len() != back_to {
        assert!(t.elapsed() < Duration::from_secs(10), "the revert");
        std::thread::sleep(Duration::from_millis(50));
    }
    wait(&s, "the revert");
    let reverted_at = captured(&log).len();

    // 2. The second: over the first one's summary, replacing tool output
    // that one cleared.
    let next = grow(&s, next, 2);
    let r = summaries();
    let gone = check("background summary 2", &s, &b, &r[1]);
    assert!(!gone.is_empty(), "summary 2 replaces output summary 1 cleared");

    // 3. Saved and loaded again, the third.
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let id = s.conv.lock().unwrap().id.clone();
    drop(s);
    crate::store::flush();
    crate::store::remove_conversation(&id);
    let mut reloaded: Conversation = serde_json::from_str(&saved).unwrap();
    reloaded.id = format!("{id}-reloaded");
    let s = Session::start(reloaded, eframe::egui::Context::default());
    let b = before(&s);
    grow(&s, next, 3);
    check("background summary 3, after a reload", &s, &b, &summaries()[2]);
    assert!(
        captured(&log)[reverted_at..].iter().all(|r| !r["body"].to_string().contains("SENT-REVERT")),
        "what was reverted is never sent again"
    );
    let id = s.conv.lock().unwrap().id.clone();
    drop(s);
    crate::store::flush();
    crate::store::remove_conversation(&id);

    // 4. The blocking summary, when a request is refused as too long.
    rolling::set_config(rolling::Config { trigger: 2_000_000, keep: 30_000 });
    let prior: Vec<Value> = (0..20).flat_map(|i| turn(i + 100, i % 5 == 3)).collect();
    let before_count = summaries().len();
    let s = Session::start(conv("overflow", prior), eframe::egui::Context::default());
    let b = before(&s);
    s.send("SENT-BLOCK-99 This request is refused as too long once.".into(), vec![]);
    wait(&s, "the blocking summary");
    let r = summaries();
    assert_eq!(r.len(), before_count + 1, "a blocking summary request");
    check("blocking summary", &s, &b, &r[before_count]);
    assert!(!r[before_count]["body"].to_string().contains("SENT-BLOCK-99"), "the refused message stays out");
    let id = s.conv.lock().unwrap().id.clone();
    drop(s);
    crate::store::flush();
    crate::store::remove_conversation(&id);
    rolling::set_config(rolling::DEFAULT);
}
