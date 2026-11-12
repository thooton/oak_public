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
