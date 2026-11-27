//! Opt-in tool-turnaround benchmark, against a fake API.

use super::*;

/// How long OAK takes around a tool call when the model takes no time:
/// conversations driven, as the app drives them, against
/// `bench/tools/fake_anthropic.py` (in the soupskin-b2/oak repository),
/// which answers instantly with scripted EXECUTE `true`, READ and WRITE
/// calls and times each from the reply that carried it to the request
/// that brought its result. With `OAK_BENCH_LOG` the timeline adds where
/// that time goes (tool_start, tool_end, request_built). Its account is a
/// throwaway API key in `OAK_DIR`, which must be a scratch folder:
///
/// ```text
/// OAK_DIR=$(mktemp -d) OAK_API_BASE=http://127.0.0.1:5701 OAK_BENCH_LOG=/tmp/oak-timeline.jsonl \
///   cargo test --release bench_tool_turnaround -- --ignored --nocapture
/// ```
///
/// `OAK_TOOLBENCH_RUNS` (5), `OAK_TOOLBENCH_STEPS` (20),
/// `OAK_TOOLBENCH_SCENARIOS` (exec,read,write,par4) and
/// `OAK_TOOLBENCH_ONE_CHAT` (1: every run in one conversation) shape it.
#[test]
#[ignore]
fn bench_tool_turnaround() {
    let base = std::env::var("OAK_API_BASE").expect("OAK_API_BASE: the fake API's address");
    assert!(base.starts_with("http://127.0.0.1") || base.starts_with("http://localhost"), "only against a local fake API");
    let dir = std::env::var_os("OAK_DIR").expect("OAK_DIR: a scratch folder (it gets a throwaway account)");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        std::path::Path::new(&dir).join("accounts.json"),
        json!({"list": [{"id": 1, "auth": {"type": "api_key", "key": "sk-ant-bench"}}], "current": 1, "next_id": 2}).to_string(),
    )
    .unwrap();
    assert!(api::load_auth());
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
    let runs: usize = env("OAK_TOOLBENCH_RUNS", "5").parse().unwrap();
    let steps: usize = env("OAK_TOOLBENCH_STEPS", "20").parse().unwrap();
    let filler: String = (0..100).map(|i| format!("line {i}: the quick brown fox jumps over the lazy dog\n")).collect();
    // A new conversation for each run, unless OAK_TOOLBENCH_ONE_CHAT=1
    // keeps them all in one, to see the cost grow with the conversation.
    let one_chat = env("OAK_TOOLBENCH_ONE_CHAT", "0") == "1";
    let mut chat: Option<(Session, String, usize)> = None;
    let mut folders = vec![];
    for r in 0..runs {
        for sc in env("OAK_TOOLBENCH_SCENARIOS", "exec,read,write,par4").split(',') {
            let folder = crate::temp_dir().join(format!("oak-toolbench-{}-{r}-{sc}", std::process::id()));
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("fixture.txt"), &filler).unwrap();
            folders.push(folder.clone());
            let run = format!("oak{r}{sc}{}", api::now_ms());
            let t = Instant::now();
            if chat.is_none() || !one_chat {
                let conv = Conversation {
                    id: format!("toolbench-{run}"),
                    title: "tool benchmark".into(),
                    folder: folder.clone(),
                    model: "claude-opus-4-5".into(),
                    effort: "low".into(),
                    ..Default::default()
                };
                let id = conv.id.clone();
                chat = Some((Session::start(conv, eframe::egui::Context::default()), id, 0));
            }
            let (s, _, replies) = chat.as_mut().unwrap();
            let want = *replies + steps + 1;
            s.send(format!("BENCH run={run} scenario={sc} steps={steps} dir={}", folder.display()), vec![]);
            loop {
                std::thread::sleep(Duration::from_millis(10));
                let n = s.conv.lock().unwrap().messages.iter().filter(|m| m["role"] == "assistant").count();
                let l = s.live.lock().unwrap();
                assert!(l.error.is_none(), "{run}: {:?}", l.error);
                if n >= want && !l.busy && l.queued.is_empty() {
                    break;
                }
                assert!(t.elapsed() < Duration::from_secs(300), "{run} timed out after {n} replies");
            }
            *replies = want;
            println!("{run}: {steps} steps in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
            if !one_chat {
                let (s, id, _) = chat.take().unwrap();
                drop(s);
                crate::store::flush();
                crate::store::remove_conversation(&id);
            }
        }
    }
    if let Some((s, id, _)) = chat.take() {
        drop(s);
        crate::store::flush();
        crate::store::remove_conversation(&id);
    }
    for f in folders {
        let _ = std::fs::remove_dir_all(f);
    }
}
