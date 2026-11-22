//! Opt-in integration tests against live providers.

use super::*;

/// Against the real API (`cargo test live_images -- --ignored --nocapture`):
/// an attached image, and an image file opened with READ.
#[test]
#[ignore]
fn live_images() {
    use base64::Engine;
    use eframe::egui::ColorImage;
    assert!(api::load_auth(), "sign in to Oak first");
    let square = |rgb: [u8; 3]| {
        let px: Vec<u8> = (0..64 * 64).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
        ColorImage::from_rgba_unmultiplied([64, 64], &px)
    };
    let model = api::latest_models().unwrap().into_iter().find(|m| m.id.contains("sonnet")).expect("a Sonnet model").id;
    let folder = std::env::temp_dir().join("oak-live-images");
    std::fs::create_dir_all(&folder).unwrap();
    let blue = crate::images::from_color_image(&square([20, 60, 230])).unwrap();
    std::fs::write(folder.join("square.png"), base64::engine::general_purpose::STANDARD.decode(&blue.data).unwrap()).unwrap();
    let conv = Conversation {
        id: format!("live-images-{}", api::now_ms()),
        title: "live images test".into(),
        folder: folder.clone(),
        model,
        effort: "low".into(),
        ..Default::default()
    };
    let id = conv.id.clone();
    let s = Session::start(conv, eframe::egui::Context::default());
    let reply = |text: &str, images: Vec<Value>| -> String {
        s.send(text.into(), images);
        let t = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let l = s.live.lock().unwrap();
            if !l.busy && l.queued.is_empty() {
                assert!(l.error.is_none(), "{:?}", l.error);
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(180), "timed out");
        }
        s.conv.lock().unwrap().messages.last().unwrap()["content"].to_string().to_lowercase()
    };
    let red = crate::images::from_color_image(&square([220, 30, 30])).unwrap().block();
    let a = reply("What colour is this image? Answer with one word.", vec![red]);
    let b = reply("Now use READ to open square.png and tell me its colour in one word.", vec![]);
    crate::store::remove_conversation(&id);
    let _ = std::fs::remove_dir_all(&folder);
    println!("attached: {a}\nread: {b}");
    assert!(a.contains("red"), "{a}");
    assert!(b.contains("blue"), "{b}");
}

/// Against the real API (run with `cargo test live_agents --
/// --ignored --nocapture`): the main session starts a subagent to find
/// a word in a file; the agent's message and final reply reach it.
#[test]
#[ignore]
fn live_agents() {
    assert!(api::load_auth(), "sign in to Oak first");
    let model = api::latest_models().unwrap().into_iter().find(|m| m.id.contains("haiku")).expect("a Haiku model").id;
    let id = format!("live-agents-{}", api::now_ms());
    let folder = std::env::temp_dir().join(&id);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("secret.txt"), "The password is: periwinkle-42\n").unwrap();
    let conv = Conversation { id: id.clone(), folder: folder.clone(), model, effort: "low".into(), ..Default::default() };
    let s = Session::start(conv, eframe::egui::Context::default());
    s.send(
        "Test of subagents: start one subagent with AGENT_NEW and have it read secret.txt in the working folder. Tell it to \
         send you a short progress message with AGENT_SEND first, then end with the password as its final reply. Don't read \
         the file yourself. When its final reply arrives, tell me the password."
            .into(),
        vec![],
    );
    let child_id = format!("{id}-agent1");
    let t = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        assert!(t.elapsed() < Duration::from_secs(300), "timed out");
        let l = s.live.lock().unwrap();
        assert!(l.error.is_none(), "{:?}", l.error);
        let idle = !l.busy && l.queued.is_empty();
        drop(l);
        // (One lock at a time: workers take the conversation's first.)
        let idle = idle && s.conv.lock().unwrap().inbox.is_empty();
        let child_idle = find_session(&child_id).is_some_and(|c| {
            let busy = c.live.lock().unwrap().busy;
            !busy && c.conv.lock().unwrap().inbox.is_empty()
        });
        let said = s.conv.lock().unwrap().messages.last().is_some_and(|m| m["role"] == "assistant" && m.to_string().contains("periwinkle-42"));
        if idle && child_idle && said {
            break;
        }
    }
    let c = s.conv.lock().unwrap().clone();
    let child = find_session(&child_id).unwrap().conv.lock().unwrap().clone();
    let names = |c: &Conversation| -> Vec<String> {
        c.messages.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).filter(|b| b["type"] == "tool_call").map(|b| b["name"].as_str().unwrap_or("").to_string()).collect()
    };
    let texts: Vec<String> = c.messages.iter().filter(|m| m["role"] == "user").flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).filter_map(|b| b["text"].as_str().map(str::to_string)).collect();
    let from_agent: Vec<_> = texts.iter().filter_map(|t| parse_agent_message(t)).filter(|m| m.0 == "agent 1 (".to_string() + &child.title + ")").collect();
    assert!(names(&c).contains(&"AGENT_NEW".to_string()) && !names(&c).contains(&"READ".to_string()));
    assert!(names(&child).contains(&"AGENT_SEND".to_string()), "the agent sent a message");
    assert!(from_agent.iter().any(|m| !m.1), "a progress message");
    assert!(from_agent.iter().any(|m| m.1 && m.2.contains("periwinkle-42")), "the final reply");
    crate::store::remove_conversation(&id);
    crate::store::remove_conversation(&child_id);
    let _ = std::fs::remove_dir_all(&folder);
}

/// Against OpenRouter (`cargo test live_openrouter -- --ignored
/// --nocapture`, with `OAK_DIR` a scratch folder holding a copy of
/// openrouter.json): real sessions on each model (`OAK_TEST_MODELS`,
/// comma-separated; the built-in ones but DeepSeek by default), the models
/// side by side, `OAK_TEST_RUNS` times each (default 3). Each run is a task
/// of several tool calls whose result is checked; every retry and error
/// the session showed is printed, and the last request body of each model
/// goes to `OAK_TEST_DUMP` (when set) for replaying.
#[test]
#[ignore]
fn live_openrouter() {
    assert!(std::env::var_os("OAK_DIR").is_some(), "set OAK_DIR to a scratch folder");
    assert!(crate::openrouter::KEYS.load(), "no OpenRouter key in OAK_DIR");
    let models: Vec<String> = std::env::var("OAK_TEST_MODELS")
        .map(|m| m.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_else(|_| crate::openrouter::MODELS[1..].iter().map(|m| m.id.to_string()).collect());
    let runs: usize = std::env::var("OAK_TEST_RUNS").ok().and_then(|r| r.parse().ok()).unwrap_or(3);
    let threads: Vec<_> = models
        .iter()
        .cloned()
        .map(|model| {
            std::thread::spawn(move || {
                let mut lines = vec![];
                let mut ok = 0;
                for run in 0..runs {
                    let id = format!("live-or-{}-{run}-{}", model.replace(['/', ':', '.'], "-"), api::now_ms());
                    let folder = std::env::temp_dir().join(&id);
                    std::fs::create_dir_all(&folder).unwrap();
                    let nums: Vec<u64> = (0..12).map(|i| (i * 37 + run as u64 * 11) % 97 + 3).collect();
                    let sum: u64 = nums.iter().sum();
                    std::fs::write(folder.join("numbers.txt"), nums.iter().map(|n| format!("{n}\n")).collect::<String>()).unwrap();
                    let conv = Conversation { id: id.clone(), folder: folder.clone(), model: model.clone(), effort: "high".into(), ..Default::default() };
                    let s = Session::start(conv, eframe::egui::Context::default());
                    s.send(
                        "Test of your tools. 1) Use EXECUTE to run `wc -l numbers.txt`. 2) READ numbers.txt. 3) WRITE sum.txt holding only \
                         the sum of the numbers in numbers.txt. 4) READ sum.txt back to check it. Then reply with one line: DONE and the sum."
                            .into(),
                        vec![],
                    );
                    let t = Instant::now();
                    let (mut seen, mut last): (Vec<String>, String) = (vec![], String::new());
                    let error = loop {
                        std::thread::sleep(Duration::from_millis(100));
                        let l = s.live.lock().unwrap();
                        let status = l.status.clone();
                        let (idle, error) = (!l.busy && l.queued.is_empty(), l.error.clone());
                        drop(l);
                        let low = status.to_lowercase();
                        if (low.contains("retry") || low.contains("error") || low.contains("rate")) && status != last {
                            seen.push(format!("{:>5.1}s {status}", t.elapsed().as_secs_f32()));
                        }
                        last = status;
                        if idle || error.is_some() {
                            break error;
                        }
                        if t.elapsed() > Duration::from_secs(900) {
                            break Some("timed out (15 min)".into());
                        }
                    };
                    let c = s.conv.lock().unwrap().clone();
                    let calls: Vec<String> = c.messages.iter().flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).filter(|b| b["type"] == "tool_call").map(|b| b["name"].as_str().unwrap_or("").to_string()).collect();
                    let written = std::fs::read_to_string(folder.join("sum.txt")).unwrap_or_default();
                    let right = written.trim() == sum.to_string();
                    let last = c.messages.last().map(|m| m["content"].to_string()).unwrap_or_default();
                    let said = last.contains(&sum.to_string());
                    let good = error.is_none() && right;
                    ok += good as usize;
                    lines.push(format!(
                        "  run {run}: {} in {:.0}s, {} calls [{}], sum.txt {}, reply {}, served by {:?}{}",
                        if good { "OK" } else { "FAILED" },
                        t.elapsed().as_secs_f32(),
                        calls.len(),
                        calls.join(" "),
                        if right { "right" } else { "wrong" },
                        if said { "names the sum" } else { "doesn't name the sum" },
                        c.served_by,
                        error.map(|e| format!("\n    ERROR: {}", e.replace('\n', " | "))).unwrap_or_default(),
                    ));
                    lines.extend(seen.iter().map(|s| format!("    {s}")));
                    println!("{model}\n{}", lines[lines.len() - 1 - seen.len()..].join("\n"));
                    s.interrupt();
                    crate::store::remove_conversation(&id);
                    let _ = std::fs::remove_dir_all(&folder);
                }
                (model, ok, lines)
            })
        })
        .collect();
    let mut all_ok = true;
    for t in threads {
        let (model, ok, lines) = t.join().unwrap();
        all_ok &= ok == runs;
        println!("{model}: {ok}/{runs}\n{}", lines.join("\n"));
    }
    if let Ok(dir) = std::env::var("OAK_TEST_DUMP") {
        std::fs::create_dir_all(&dir).unwrap();
        for (i, b) in crate::openrouter::SENT.lock().unwrap().iter().enumerate() {
            let name = b["model"].as_str().unwrap_or("x").replace(['/', ':'], "_");
            std::fs::write(format!("{dir}/{i:03}-{name}.json"), serde_json::to_string(b).unwrap()).unwrap();
        }
    }
    assert!(all_ok, "some runs failed");
}

/// Against the real service (run with `cargo test live_freebuff --
/// --ignored --nocapture`): one turn through the Freebuff sidecar.
#[test]
#[ignore]
fn live_freebuff() {
    assert!(crate::freebuff::load(), "no Freebuff login (freebuff.json)");
    let id = format!("live-freebuff-{}", api::now_ms());
    let conv = Conversation {
        id: id.clone(),
        folder: std::env::var("OAK_TEST_FOLDER").map(Into::into).unwrap_or_else(|_| std::env::temp_dir()),
        model: "deepseek/deepseek-v4-flash".into(),
        effort: "low".into(),
        ..Default::default()
    };
    let s = Session::start(conv, eframe::egui::Context::default());
    let prompt = std::env::var("OAK_TEST_PROMPT").unwrap_or_else(|_| "Reply with exactly the token PINEAPPLE-42 and nothing else.".into());
    s.send(prompt, vec![]);
    let t = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let l = s.live.lock().unwrap();
        let err = l.error.clone();
        let idle = !l.busy && l.queued.is_empty();
        drop(l);
        if let Some(e) = err {
            panic!("{e}");
        }
        if idle {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(180), "timed out");
    }
    let c = s.conv.lock().unwrap().clone();
    for m in &c.messages {
        println!("{m}\n");
    }
    println!("AS SENT: {}", serde_json::to_string_pretty(&crate::freebuff::chat_messages(&c.messages)).unwrap());
    crate::store::remove_conversation(&id);
    crate::freebuff::release();
}
