//! Frame-cost benchmarks of the whole app (run on their own, ignored).

use super::*;
use std::time::Instant;

/// A long session as OAK keeps it: `turns` rounds of a markdown reply
/// with a code block, an EXECUTE call and its (long) output.
pub(crate) fn long_conversation(id: &str, turns: usize) -> serde_json::Value {
    let mut messages = vec![];
    for i in 0..turns {
        messages.push(serde_json::json!({"role": "user", "content": [{"type": "text", "text": format!("Step {i}: look at the parser and fix what fails.")}]}));
        let reply = format!(
            "## Step {i}\n\nThe parser **fails** on `nested` input, because the *lexer* drops a token:\n\n- first point about `tokens`\n- second point, with a [link](https://example.com)\n\n```rust\nfn parse(input: &str) -> Result<Ast, Error> {{\n    let tokens = lex(input)?;\n    let mut p = Parser::new(&tokens);\n    p.expr(0)\n}}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nNext I run the tests."
        );
        let call = format!("call{i}");
        messages.push(serde_json::json!({"role": "assistant", "content": [
            {"type": "text", "text": reply},
            {"type": "tool_call", "id": call, "name": "EXECUTE", "input": {"command": "cargo test", "timeout": 60, "summary": "Run the tests"}}]}));
        let output: String = (0..400).map(|l| format!("test parser::case_{l} ... ok\n")).collect();
        messages.push(serde_json::json!({"role": "user", "content": [{"type": "tool_result", "call_id": call, "content": output}]}));
    }
    // A long goal session's lines: a files-changed notice every
    // other turn and the goal continued every fifth.
    let marks: Vec<_> = (0..turns)
        .filter(|i| i % 2 == 0 || i % 5 == 0)
        .map(|i| {
            let kind = if i % 2 == 0 { "files_changed" } else { "goal_continued" };
            serde_json::json!({"at": i * 3 + 2, "kind": kind, "text": "src/parser.rs", "time": 0, "done": 0, "sent": "changed"})
        })
        .collect();
    serde_json::json!({"id": id, "title": format!("Conversation {id}"), "folder": "/tmp/x", "model": "m", "effort": "low",
        "messages": messages, "marks": marks, "last_message_at": 0, "next_task_id": 0, "format": crate::oak::FORMAT})
}

/// What a frame costs: startup with many conversations on disk, Home's
/// chats list, a long transcript at its end and scrolled, and whether
/// OAK asks for another frame while nothing changes. Run on its own:
/// `cargo test --release bench_frames -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_frames() {
    let dir = std::env::temp_dir().join(format!("oak-frame-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("auth.json"), r#"{"type":"api_key","key":"not-a-real-key"}"#).unwrap();
    // Under test, conversations live in the test run's own folder.
    let convs = store::conversations_dir();
    let mut bytes = 0;
    let turns_of = |name: &str, default: usize| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    for n in 0..200 {
        let turns = if n == 0 { turns_of("BENCH_TURNS", 300) } else { turns_of("BENCH_OTHERS", 20) };
        let id = 1_700_000_000_000u64 + n;
        let c = long_conversation(&id.to_string(), turns).to_string();
        bytes += c.len();
        std::fs::write(convs.join(format!("x - Conversation {id} - {id}.json")), c).unwrap();
    }
    unsafe { std::env::set_var("OAK_DIR", &dir) };
    let t = Instant::now();
    let n = store::load_conversations().len();
    println!("reading {n} conversations: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    let ctx = egui::Context::default();
    let t = Instant::now();
    let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()));
    let startup = t.elapsed();
    // (Linux: what the process holds, the conversations among it.)
    let rss = || std::fs::read_to_string("/proc/self/status").ok()?.lines().find_map(|l| l.strip_prefix("VmRSS:")).map(|v| v.trim().to_string());
    let parsed = || store::PARSED.load(std::sync::atomic::Ordering::Relaxed);
    println!("memory after startup: {} ({} conversations' messages parsed)", rss().unwrap_or_default(), parsed());
    let mut frame = eframe::Frame::_new_kittest();
    let mut time = 0.0;
    let mut run = |app: &mut App, events: Vec<egui::Event>| {
        time += 1.0 / 60.0;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1384.0, 821.0))), time: Some(time), events, ..Default::default() };
        let t = Instant::now();
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        let took = t.elapsed();
        out.textures_delta.clear();
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay);
        (took, delay)
    };
    let stats = |name: &str, v: &[(Duration, Duration)]| {
        let mut ms: Vec<f64> = v.iter().map(|(d, _)| d.as_secs_f64() * 1000.0).collect();
        ms.sort_by(f64::total_cmp);
        let again = v.iter().filter(|(_, d)| d.is_zero()).count();
        println!("{name:<28} median {:>7.3} ms  p90 {:>7.3} ms  max {:>7.3} ms  asked again at once {again}/{}", ms[ms.len() / 2], ms[ms.len() * 9 / 10], ms[ms.len() - 1], v.len());
    };
    println!("startup with {} conversations ({:.1} MB): {:.1} ms", app.sessions.len(), bytes as f64 / 1e6, startup.as_secs_f64() * 1000.0);
    let first = run(&mut app, vec![]);
    println!("first frame (Home): {:.1} ms", first.0.as_secs_f64() * 1000.0);
    let home: Vec<_> = (0..120).map(|_| run(&mut app, vec![])).collect();
    stats("Home, idle", &home[60..]);
    println!("  messages parsed by then: {}", parsed());
    app.open("1700000000000".into());
    let t = Instant::now();
    let open = run(&mut app, vec![]);
    println!("open the long conversation: {:.1} ms (frame {:.1} ms)", t.elapsed().as_secs_f64() * 1000.0, open.0.as_secs_f64() * 1000.0);
    {
        // The per-frame steps of chat_view before anything is drawn.
        let s = &app.sessions["1700000000000"];
        let conv = s.conv.lock().unwrap();
        let live = s.live.lock().unwrap();
        let time = |name: &str, f: &mut dyn FnMut()| {
            let t = Instant::now();
            for _ in 0..50 {
                f();
            }
            println!("  {name:<20} {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / 50.0);
        };
        time("items_at", &mut || drop(std::hint::black_box(items_at(&conv, &live))));
        let (items, _, results) = items_at(&conv, &live);
        time("units", &mut || drop(std::hint::black_box(units(&items))));
        let u = units(&items);
        time("unit_sig (all)", &mut || drop(std::hint::black_box(u.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect::<Vec<_>>())));
    }
    let settle: Vec<_> = (0..600).map(|_| run(&mut app, vec![])).collect();
    stats("transcript, settling", &settle[..300]);
    stats("transcript, settled", &settle[300..]);
    // The last tool rows opened, their output shown.
    let last = turns_of("BENCH_TURNS", 300usize);
    for i in last - 3..last {
        ctx.data_mut(|d| d.insert_temp(Id::unique(("tool-open", format!("call{i}").as_str())), true));
    }
    let opened: Vec<_> = (0..200).map(|_| run(&mut app, vec![])).collect();
    stats("transcript, rows opened", &opened[100..]);
    let hover = vec![egui::Event::PointerMoved(egui::pos2(500.0, 400.0))];
    let _ = run(&mut app, hover);
    let scroll: Vec<_> = (0..200)
        .map(|_| run(&mut app, vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, 120.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: Modifiers::NONE }]))
        .collect();
    stats("transcript, scrolling up", &scroll);
    let still: Vec<_> = (0..200).map(|_| run(&mut app, vec![])).collect();
    stats("transcript, idle after", &still[100..]);
    // A reply streaming in at the end, a few words a frame, and the
    // conversation changing now and then (a tool's result coming in).
    let _ = run(&mut app, vec![egui::Event::Key { key: egui::Key::End, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
    let long = app.sessions["1700000000000"].clone();
    long.live.lock().unwrap().busy = true;
    let builds = crate::chat_view::BUILDS.get();
    let streaming: Vec<_> = (0..300)
        .map(|i| {
            {
                let mut l = long.live.lock().unwrap();
                let text = format!("{}word {i} of the reply, ", l.blocks.first().and_then(|b| b["text"].as_str()).unwrap_or(""));
                l.blocks = vec![serde_json::json!({"type": "text", "text": text})];
            }
            if i % 100 == 99 {
                long.conv.lock().unwrap().spent += 0.001;
            }
            run(&mut app, vec![])
        })
        .collect();
    stats("transcript, reply streaming", &streaming[50..]);
    println!("  its items worked out {} times in 300 frames (it changed 3 times)", crate::chat_view::BUILDS.get() - builds);
    {
        let mut l = long.live.lock().unwrap();
        l.busy = false;
        l.blocks.clear();
    }
    // Pictures: a session of pasted screenshots, opened after the long one.
    let shot = |seed: u32| {
        let img = image::RgbaImage::from_fn(1400, 900, |x, y| {
            let v = ((x / 7) ^ (y / 5)).wrapping_mul(seed + 3) as u8;
            image::Rgba([v, v.wrapping_add(40), 200 - (v / 2), 255])
        });
        let mut png = vec![];
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        images::from_bytes(&png).unwrap().block()
    };
    let mut messages = vec![];
    let mut data = 0;
    for i in 0..40 {
        let b = shot(i);
        data += b["data"].as_str().map_or(0, str::len);
        messages.push(serde_json::json!({"role": "user", "content": [b, {"type": "text", "text": format!("What is wrong in screenshot {i}?")}]}));
        messages.push(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "The header overlaps the list."}]}));
    }
    let conv = Conversation { id: "pictures".into(), title: "Pictures".into(), messages: messages.into(), ..Default::default() };
    app.sessions.insert("pictures".into(), Session::start(conv, ctx.clone()));
    let textures = |ctx: &egui::Context| ctx.tex_manager().read().num_allocated();
    let before = textures(&ctx);
    app.open("pictures".into());
    let open: Vec<_> = (0..300).map(|_| run(&mut app, vec![])).collect();
    stats("pictures, opening", &open[..60]);
    stats("pictures, settled", &open[200..]);
    println!("pictures: 40 screenshots, {:.1} MB of base64; textures held: {} more than before opening", data as f64 / 1e6, textures(&ctx) - before);
    let _ = run(&mut app, vec![egui::Event::PointerMoved(egui::pos2(500.0, 400.0))]);
    let scroll: Vec<_> = (0..400)
        .map(|_| run(&mut app, vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, 60.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: Modifiers::NONE }]))
        .collect();
    stats("pictures, scrolling up", &scroll);
    let slow = scroll.iter().filter(|(d, _)| d.as_secs_f64() > 1.0 / 60.0).count();
    println!("pictures, scrolling up: {slow} of 400 frames over 16.7 ms; textures held: {} more than before opening", textures(&ctx) - before);
    app.open("1700000000000".into());
    for _ in 0..300 {
        run(&mut app, vec![]);
    }
    println!("after going back to the long conversation for 5 s: {} more textures than before", textures(&ctx) - before);
    for _ in 0..60 * 62 {
        run(&mut app, vec![]);
    }
    println!("after 67 s: {} more textures than before", textures(&ctx) - before);
    let _ = std::fs::remove_dir_all(&dir);
}


/// What the transcript works out every frame before drawing, for a
/// 300- and a 3000-turn conversation, with and without old tool output
/// cleared at a cold moment. Run on its own:
/// `cargo test --release bench_transcript_steps -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_transcript_steps() {
    for (turns, cold) in [(300, false), (3000, false), (300, true), (3000, true)] {
        let mut conv: Conversation = serde_json::from_value(long_conversation("1", turns)).unwrap();
        if cold {
            let Conversation { rolling, messages, .. } = &mut conv;
            assert!(rolling.clear_cold(messages));
        }
        let live = Live::default();
        let label = format!("{turns} turns{}", if cold { ", cold-cleared" } else { "" });
        // The median of 15 runs of 10.
        let time = |name: &str, f: &mut dyn FnMut()| {
            let mut v: Vec<f64> = (0..15)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..10 {
                        f();
                    }
                    t.elapsed().as_secs_f64() * 100.0
                })
                .collect();
            v.sort_by(f64::total_cmp);
            println!("{label:<26} {name:<14} {:.3} ms", v[7]);
        };
        time("cleared_calls", &mut || drop(std::hint::black_box(conv.rolling.cleared_calls(&conv.messages))));
        time("items_at", &mut || drop(std::hint::black_box(items_at(&conv, &live))));
        let (items, _, results) = items_at(&conv, &live);
        let u = units(&items);
        time("unit_sig (all)", &mut || drop(std::hint::black_box(u.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect::<Vec<_>>())));
        time("all", &mut || {
            let (items, _, results) = items_at(&conv, &live);
            let u = units(&items);
            std::hint::black_box(u.iter().map(|u| unit_sig(&items[u.clone()], &results)).collect::<Vec<_>>());
        });
    }
}



/// A long reply streaming in at the end of a 300-turn conversation: about
/// 40 kB of markdown (headings, lists, code blocks, a table), a chunk of
/// 60 bytes a frame, as a long final answer arrives. What a frame costs
/// early (under 10 kB in), midway and late (over 30 kB in), and once it
/// has all come. Run on its own:
/// `cargo test --release bench_streaming_reply -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_streaming_reply() {
    let dir = std::env::temp_dir().join(format!("oak-stream-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("auth.json"), r#"{"type":"api_key","key":"not-a-real-key"}"#).unwrap();
    let convs = store::conversations_dir();
    let id = "1700000000000";
    std::fs::write(convs.join(format!("x - Conversation {id} - {id}.json")), long_conversation(id, 300).to_string()).unwrap();
    unsafe { std::env::set_var("OAK_DIR", &dir) };
    let ctx = egui::Context::default();
    let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()));
    let mut frame = eframe::Frame::_new_kittest();
    let mut time = 0.0;
    let mut run = |app: &mut App, events: Vec<egui::Event>| {
        time += 1.0 / 60.0;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1384.0, 821.0))), time: Some(time), events, ..Default::default() };
        let t = Instant::now();
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        let took = t.elapsed();
        out.textures_delta.clear();
        took.as_secs_f64() * 1000.0
    };
    let stats = |name: &str, mut ms: Vec<f64>| {
        ms.sort_by(f64::total_cmp);
        let mean = ms.iter().sum::<f64>() / ms.len() as f64;
        println!("{name:<34} median {:>7.3} ms  p90 {:>7.3} ms  mean {:>7.3} ms  ({} frames)", ms[ms.len() / 2], ms[ms.len() * 9 / 10], mean, ms.len());
    };
    app.open(id.into());
    for _ in 0..120 {
        run(&mut app, vec![]);
    }
    let mut reply = String::new();
    for i in 0.. {
        reply.push_str(&format!(
            "## Part {i}\n\nThe change to `parser.rs` **moves** the lexer's lookahead into the *parser*, so a token is read once. \
             Each step below was checked against the tests in `tests/parse.rs`, which now pass.\n\n\
             - the first point, about `tokens` and [their spans](https://example.com/spans/{i})\n\
             - the second point, which is longer and wraps onto another line when the window is narrow enough\n\
             - the third point\n\n\
             ```rust\nfn parse_{i}(input: &str) -> Result<Ast, Error> {{\n    let tokens = lex(input)?;\n    let mut p = Parser::new(&tokens);\n    p.expr(0)\n}}\n```\n\n\
             | case | before | after |\n|---|---|---|\n| nested | fails | passes |\n| empty | passes | passes |\n\n"
        ));
        if reply.len() > 40_000 {
            break;
        }
    }
    let s = app.sessions[id].clone();
    s.live.lock().unwrap().busy = true;
    let (mut early, mut mid, mut late) = (vec![], vec![], vec![]);
    let mut at = 0;
    while at < reply.len() {
        at = (at + 60).min(reply.len());
        while !reply.is_char_boundary(at) {
            at += 1;
        }
        s.live.lock().unwrap().blocks = vec![serde_json::json!({"type": "text", "text": &reply[..at]})];
        let ms = run(&mut app, vec![]);
        match at {
            ..10_000 => early.push(ms),
            10_000..30_000 => mid.push(ms),
            _ => late.push(ms),
        }
    }
    println!("a {:.1} kB reply streaming in, 60 bytes a frame:", reply.len() as f64 / 1000.0);
    let all: Vec<f64> = early.iter().chain(&mid).chain(&late).copied().collect();
    let total: f64 = all.iter().sum();
    stats("  under 10 kB in", early);
    stats("  10 to 30 kB in", mid);
    stats("  over 30 kB in", late);
    println!("  {:<32} {total:.0} ms over {} frames", "all of it", all.len());
    let still: Vec<f64> = (0..200).map(|_| run(&mut app, vec![])).collect();
    stats("  all in, still streaming", still[100..].to_vec());
    let _ = std::fs::remove_dir_all(&dir);
}
