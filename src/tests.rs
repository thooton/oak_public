//! The app's tests.

use super::*;
use crate::app_hover_test::{click, copied, key, painted, press, rect_of, run_frame, test_ctx};
use egui::PointerButton::{Primary, Secondary};
use home::*;
use tabs::TabClose;
use transcript::*;

/// Which tabs each close command closes, in the order they're drawn.
#[test]
fn tab_close_targets_follow_visual_order() {
    let targets = |action: TabClose, anchor, count| (0..count).filter(|&i| action.includes(i, anchor)).collect::<Vec<_>>();
    assert_eq!(targets(TabClose::One, 1, 3), [1]);
    assert_eq!(targets(TabClose::Others, 1, 3), [0, 2]);
    assert_eq!(targets(TabClose::Left, 1, 3), [0]);
    assert_eq!(targets(TabClose::Right, 1, 3), [2]);
    assert_eq!(targets(TabClose::All, 1, 3), [0, 1, 2]);
    assert!(targets(TabClose::Left, 0, 3).is_empty() && targets(TabClose::Right, 2, 3).is_empty());
    assert!(targets(TabClose::Others, 0, 1).is_empty());
}

#[test]
fn cloud_views_draw() {
    use serde_json::json;
    let (ctx, t) = test_ctx(false, false);
    let key = cloud::key("uuid-test", "session_test1");
    let f = |seq: u64, source: &str, p: Value| json!({"sequence_num": seq, "source": source, "payload": p}).to_string();
    cloud::test_chat(
        &key,
        &[
            f(1, "client", json!({"type": "user", "uuid": "u", "message": {"content": "<oak-setup>\nx\n</oak-setup>\n\nfix it"}})),
            f(2, "worker", json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Looking."}, {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls", "description": "List files"}}]}})),
            f(3, "worker", json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "a"}]}})),
            f(4, "worker", json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "**Done.**"}]}})),
            f(5, "worker", json!({"type": "result", "subtype": "success"})),
        ],
    );
    let mut list = vlist::List::default();
    let (mut texts, mut time) = (vec![], 0.0);
    for _ in 0..3 {
        let out = run_frame(&ctx, vec2(1200.0, 700.0), &mut time, 1.0 / 60.0, vec![], |ui| cloud_chat_view(ui, &t, &key, &mut list));
        texts = painted(&out).into_iter().map(|(s, _)| s).collect();
    }
    let all = texts.join("|");
    for want in ["fix it", "Looking.", "List files", "Done."] {
        assert!(all.contains(want), "{want} not drawn: {all}");
    }
    assert!(!all.contains("oak-setup"), "{all}");
    assert_eq!(cloud::tab_info(&key), ("A test session".to_string(), false));
}

/// A right-click on a conversation offers "Delete conversation", and
/// picking it asks to delete that conversation (not open it); a list
/// whose rows can't be deleted (cloud sessions) offers nothing.
#[test]
fn chat_list_delete_menu() {
    let (ctx, t) = test_ctx(false, false);
    let now = api::now_ms();
    let rows: Vec<ChatRow> = DUMMY_CHATS
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, (title, folder, mins))| ChatRow {
            id: i.to_string(),
            title: title.to_string(),
            folder: folder.to_string(),
            at: now - mins * 60_000,
            busy: false,
            paused: false,
            renamable: true,
        })
        .collect();
    let mut time = 0.0;
    let mut run = |can_delete: bool, events: Vec<egui::Event>| {
        let mut action = ChatListAction::default();
        let out = run_frame(&ctx, vec2(900.0, 500.0), &mut time, 1.0 / 60.0, events, |ui| action = chat_list(ui, &t, &rows, can_delete));
        let menu = painted(&out).into_iter().rfind(|(s, _)| s == "Delete conversation").map(|(_, r)| r.center());
        (action, menu)
    };
    let at = egui::pos2(300.0, 87.0);
    for can_delete in [false, true] {
        run(can_delete, vec![]);
        run(can_delete, vec![egui::Event::PointerMoved(at)]);
        run(can_delete, click(at, Secondary));
        let (action, menu) = run(can_delete, vec![egui::Event::PointerGone]);
        if !can_delete {
            // Cloud sessions, which aren't Oak conversations: no menu.
            assert!(menu.is_none(), "cloud rows offer no Delete conversation");
            continue;
        }
        // A conversation: right-click opens the menu.
        assert!(action.open.is_none() && action.delete.is_none(), "right-click neither opens nor deletes");
        let menu = menu.expect("no Delete conversation in the menu");
        run(true, vec![egui::Event::PointerMoved(menu)]);
        let (action, _) = run(true, click(menu, Primary));
        assert_eq!(action.delete.as_deref(), Some("1"), "the second row's conversation is deleted");
        assert!(action.open.is_none(), "picking Delete doesn't open it");
    }
}

/// Unslop in the transcript: "Drafted a message" over the rewrite (the
/// model's own text only once the row is opened), the original with an
/// error when the rewrite failed, "Drafting a message…" while the reply
/// streams in, and the switch beside Settings.
#[test]
fn unslop_renders() {
    let (ctx, t) = test_ctx(true, true);
    let text = |s: &str, m: serde_json::Value| serde_json::json!({"type": "text", "text": s, "_oak_unslop": m});
    let conv = Conversation {
        id: "unslop-render".into(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "fixed?"}]}),
            serde_json::json!({"role": "assistant", "content": [
                text("Looking now.", serde_json::json!({"id": "a", "text": "I am taking a careful look now."})),
                {"type": "tool_call", "id": "t1", "name": "READ", "input": {"path": "x"}}]}),
            serde_json::json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "ok"}]}),
            serde_json::json!({"role": "assistant", "content": [text("Ship it.", serde_json::json!({"id": "b", "error": "HTTP 500"}))]}),
        ].into(),
        ..Default::default()
    };
    unslop::set(true);
    let mut live = Live::default();
    live.busy = true;
    live.blocks = vec![serde_json::json!({"type": "text", "text": "Half a reply"})];
    let (mut texts, mut time, mut row) = (vec![], 0.0, None);
    for frame in 0..4 {
        // Opens the first "Drafted a message" row on the third frame.
        let events = if frame == 2 { click(row.unwrap(), Primary) } else { vec![] };
        let shapes = painted(&run_frame(&ctx, vec2(1000.0, 1500.0), &mut time, 1.0 / 60.0, events, |ui| transcript_view(ui, &t, &conv, &live)));
        let t2: Vec<String> = shapes.iter().map(|(s, _)| s.clone()).collect();
        if frame == 1 {
            row = Some(rect_of(&shapes, "Drafted a message").center());
            let has = |s: &str| t2.iter().any(|x| x.contains(s));
            assert!(has("Drafted a message") && has("I am taking a careful look now.") && !has("Looking now."), "{t2:?}");
            assert!(has("Ship it.") && has("Rewrite failed: HTTP 500"), "{t2:?}");
            assert!(has("Drafting a message…") && !has("Half a reply"), "{t2:?}");
        }
        texts = t2;
    }
    unslop::set(false);
    assert!(texts.iter().any(|x| x.contains("Looking now.")), "opened, the row shows the model's own text: {texts:?}");
}

/// Right-clicking one of the model's items (a tool call, its text)
/// opens a menu whose entry asks the app to revert to before it.
#[test]
fn revert_menu_on_the_models_items() {
    let (ctx, t) = test_ctx(true, true);
    let call = |id: &str| serde_json::json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": format!("{id}.txt")}});
    let result = |id: &str| serde_json::json!({"type": "tool_result", "call_id": id, "content": "ok"});
    let conv = Conversation {
        id: "revert-menu".into(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "look"}]}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Reading."}, call("alpha"), call("beta")]}),
            serde_json::json!({"role": "user", "content": [result("alpha"), result("beta")]}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Finished here."}]}),
        ].into(),
        ..Default::default()
    };
    let live = Live::default();
    let mut time = 0.0;
    let mut frame = |events| painted(&run_frame(&ctx, vec2(1000.0, 900.0), &mut time, 1.0 / 60.0, events, |ui| transcript_view(ui, &t, &conv, &live)));
    let at = |shapes: &[(String, Rect)], s: &str| shapes.iter().find(|(x, _)| x.contains(s)).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {s:?} in {shapes:?}"));
    let mut pick = |target: &str| {
        let shapes = frame(vec![]);
        assert!(!shapes.iter().any(|(x, _)| x == "Revert to before this"), "no menu before a right-click");
        let p = at(&shapes, target);
        frame(vec![egui::Event::PointerMoved(p)]);
        frame(click(p, Secondary));
        frame(vec![]);
        let item = at(&frame(vec![]), "Revert to before this");
        frame(vec![egui::Event::PointerMoved(item)]);
        frame(click(item, Primary));
        frame(vec![egui::Event::PointerGone]);
        ctx.data_mut(|d| d.remove_temp::<Option<agent::RevertTo>>(Id::unique(REWIND_REQUEST))).flatten()
    };
    assert_eq!(pick("beta.txt"), Some(agent::RevertTo::Block(1, 2, call("beta"))));
    assert_eq!(pick("Finished here."), Some(agent::RevertTo::Block(3, 0, conv.messages[3]["content"][0].clone())));
}

/// Right-clicking a reply's selected text keeps the selection and offers
/// Copy (it), Copy message, Copy link address over a link, and Revert to
/// before this; the user's message has the copies too.
#[test]
fn copy_menu_on_transcript_text() {
    let (ctx, t) = test_ctx(true, true);
    copy_menu::install(&ctx);
    let conv = Conversation {
        id: "copy-menu".into(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "show me"}]}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Finished here, see [the docs](https://example.com/docs) now."}]}),
        ].into(),
        ..Default::default()
    };
    let live = Live::default();
    let mut time = 0.0;
    let mut frame = |events| {
        let out = run_frame(&ctx, vec2(1000.0, 900.0), &mut time, 1.0 / 60.0, events, |ui| transcript_view(ui, &t, &conv, &live));
        (painted(&out), copied(&out))
    };
    type Frame<'a> = dyn FnMut(Vec<egui::Event>) -> (Vec<(String, Rect)>, Vec<String>) + 'a;
    let find = |shapes: &[(String, Rect)], s: &str| shapes.iter().find(|(x, _)| x == s).map(|(_, r)| *r);
    let (shapes, _) = frame(vec![]);
    let reply = shapes.iter().find(|(x, _)| x.starts_with("Finished here")).map(|(_, r)| *r).expect("the reply");
    // Select "Finished" by dragging over it.
    let (from, to) = (egui::pos2(reply.left() + 1.0, reply.center().y), egui::pos2(reply.left() + 45.0, reply.center().y));
    frame(vec![egui::Event::PointerMoved(from)]);
    frame(vec![press(from, Primary, true)]);
    frame(vec![egui::Event::PointerMoved(to)]);
    frame(vec![press(to, Primary, false)]);
    let (_, copied) = frame(vec![egui::Event::Copy]);
    let selected = copied.first().cloned().expect("a selection to copy");
    assert!(selected.starts_with("Fini") && !selected.contains("docs"), "{selected:?}");
    // Right-click in it: the menu, and the selection is still there.
    let menu = |frame: &mut Frame, at: egui::Pos2| {
        frame(vec![egui::Event::PointerMoved(at)]);
        let (_, copied) = frame(vec![press(at, Secondary, true), press(at, Secondary, false)]);
        assert!(copied.is_empty(), "a right-click copies nothing: {copied:?}");
        frame(vec![]);
        frame(vec![]).0
    };
    let shapes = menu(&mut frame, to - vec2(10.0, 0.0));
    for entry in ["Copy", "Copy message", "Revert to before this"] {
        assert!(find(&shapes, entry).is_some(), "no {entry:?} in the menu");
    }
    assert!(find(&shapes, "Copy link address").is_none(), "not over a link");
    let click = |frame: &mut Frame, at: egui::Pos2| {
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![press(at, Primary, true)]);
        let (_, copied) = frame(vec![press(at, Primary, false)]);
        frame(vec![egui::Event::PointerGone]);
        copied
    };
    assert_eq!(click(&mut frame, find(&shapes, "Copy").unwrap().center()), vec![selected.clone()]);
    // The selection outlived the menu.
    let (_, copied) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec![selected.clone()]);
    let shapes = menu(&mut frame, to - vec2(10.0, 0.0));
    assert_eq!(click(&mut frame, find(&shapes, "Copy message").unwrap().center()), vec![conv.messages[1]["content"][0]["text"].as_str().unwrap().to_string()]);
    // Over the link, with the selection cleared (a click elsewhere): its address.
    click(&mut frame, egui::pos2(990.0, 890.0));
    let (shapes, _) = frame(vec![]);
    let reply = shapes.iter().find(|(x, _)| x.starts_with("Finished here")).map(|(_, r)| *r).unwrap();
    let galley_x = |s: &str| {
        let font = egui::TextStyle::Body.resolve(&ctx.global_style());
        ctx.fonts_mut(|f| f.layout_no_wrap(s.to_string(), font, Color32::WHITE).size().x)
    };
    let link = egui::pos2(reply.left() + galley_x("Finished here, see the d"), reply.center().y);
    let shapes = menu(&mut frame, link);
    assert!(find(&shapes, "Copy").is_none(), "nothing selected now");
    assert_eq!(click(&mut frame, find(&shapes, "Copy link address").expect("Copy link address").center()), vec!["https://example.com/docs".to_string()]);
    // The user's message: copies, no revert (it has its own control).
    let (shapes, _) = frame(vec![]);
    let user = find(&shapes, "show me").unwrap();
    let shapes = menu(&mut frame, user.center());
    assert!(find(&shapes, "Copy message").is_some() && find(&shapes, "Revert to before this").is_none());
    assert_eq!(click(&mut frame, find(&shapes, "Copy message").unwrap().center()), vec!["show me".to_string()]);
}

/// Ctrl+F finds text in a conversation's messages: it counts the matches
/// (ignoring case), steps through them with Enter, scrolls each into view
/// and marks it; Escape closes it.
#[test]
fn find_in_a_conversation() {
    use serde_json::json;
    let (ctx, t) = test_ctx(false, false);
    let filler: String = (0..60).map(|i| format!("Paragraph {i} of filler.\n\n")).collect();
    let conv = store::Conversation {
        id: format!("unit-test-find-{}", api::now_ms()),
        folder: std::env::temp_dir(),
        messages: vec![
            json!({"role": "user", "content": [{"type": "text", "text": "Where is the Needle?"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": format!("{filler}Here is a **needle** at last.")}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "thanks"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "No more NEEDLE."}]}),
        ].into(),
        ..Default::default()
    };
    let id = conv.id.clone();
    let s = agent::Session::start(conv, ctx.clone());
    let mut list = vlist::List::default();
    let mut editing = None;
    let mut time = 0.0;
    // A frame of `events`, then `quiet` frames of none; the last one's
    // texts, each with where it is down the view and whether it's marked.
    let mut frame = |events: Vec<egui::Event>, quiet: usize| {
        let mut texts = vec![];
        for events in std::iter::once(events).chain((0..quiet).map(|_| vec![])) {
            let out = run_frame(&ctx, vec2(1000.0, 500.0), &mut time, 0.1, events, |ui| {
                chat_view(ui, &t, &id, &s, &mut list, &mut editing, false);
            });
            texts = out
                .shapes
                .iter()
                .filter_map(|s| if let egui::Shape::Text(x) = &s.shape { Some((x.galley.text().to_string(), x.pos.y, x.galley.job.sections.iter().any(|s| s.format.background != Color32::TRANSPARENT))) } else { None })
                .collect::<Vec<_>>();
        }
        texts
    };
    frame(vec![], 2);
    frame(vec![key(egui::Key::F, egui::Modifiers::COMMAND)], 0);
    let texts = frame(vec![egui::Event::Text("needle".into())], 4);
    let label = |texts: &[(String, f32, bool)]| texts.iter().find(|x| x.0.ends_with(" of 3") || x.0 == "No matches").map(|x| x.0.clone());
    // The view was at the end: the first match from its first unit on is
    // the one at the end of the long reply, which is in view.
    let shown = |texts: &[(String, f32, bool)], text: &str| texts.iter().find(|x| x.0 == text).is_some_and(|x| x.2 && x.1 > 0.0 && x.1 < 500.0);
    assert_eq!(label(&texts).as_deref(), Some("2 of 3"));
    assert!(shown(&texts, "Here is a needle at last."), "{texts:?}");
    // Enter goes on to the last, then round to the first, at the top,
    // scrolled into view.
    for (want, text) in [("3 of 3", "No more NEEDLE."), ("1 of 3", "Where is the Needle?")] {
        let texts = frame(vec![key(egui::Key::Enter, egui::Modifiers::NONE)], 4);
        assert_eq!(label(&texts).as_deref(), Some(want));
        assert!(shown(&texts, text), "{text} in view, marked: {texts:?}");
    }
    // Shift+Enter goes back, down the long reply again.
    let texts = frame(vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)], 4);
    assert_eq!(label(&texts).as_deref(), Some("3 of 3"));
    let texts = frame(vec![key(egui::Key::Escape, egui::Modifiers::NONE)], 1);
    assert_eq!(label(&texts), None);
    assert!(!texts.iter().any(|x| x.2), "no marks once closed");
    s.delete();
}

/// Copying a selection over a reply copies the list numbers, not the code
/// block's language; Ctrl+A in a reply's selection selects the whole reply
/// (not one paragraph), and Ctrl+C copies it as it reads.
#[test]
fn transcript_selection_copies_what_it_shows() {
    let (ctx, t) = test_ctx(true, true);
    copy_menu::install(&ctx);
    let reply = "Intro line.\n\n1. First\n2. Second\n   - nested\n\n```rust\nfn main() {}\n```\n\nOutro line.";
    let conv = Conversation {
        id: "select-copy".into(),
        messages: vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "show me"}]}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": reply}]}),
        ].into(),
        ..Default::default()
    };
    let live = Live::default();
    let mut time = 0.0;
    let mut draft = "draft field text".to_string();
    let field_id = Id::unique("selection-test-draft");
    let mut frame = |events| {
        let out = run_frame(&ctx, vec2(1000.0, 900.0), &mut time, 1.0 / 60.0, events, |ui| {
            crate::select::begin(ui);
            transcript_view(ui, &t, &conv, &live);
            text_menu::add(ui, TextEdit::singleline(&mut draft).id(field_id));
        });
        (painted(&out), copied(&out))
    };
    let (shapes, _) = frame(vec![]);
    let (from, to) = (rect_of(&shapes, "Intro line.").left_center() + vec2(1.0, 0.0), rect_of(&shapes, "Outro line.").right_center() + vec2(2.0, 0.0));
    // A drag over the whole reply.
    frame(vec![egui::Event::PointerMoved(from), press(from, Primary, true)]);
    frame(vec![egui::Event::PointerMoved((from + to.to_vec2()) / 2.0)]);
    frame(vec![egui::Event::PointerMoved(to)]);
    frame(vec![press(to, Primary, false)]);
    let (_, copied) = frame(vec![egui::Event::Copy]);
    let copied = copied.join("");
    assert!(copied.contains("1. First") && copied.contains("2. Second") && copied.contains("fn main() {}"), "{copied:?}");
    assert!(!copied.contains("rust") && !copied.contains("copy"), "no code block caption or button: {copied:?}");

    // A selection within "Second", then Ctrl+A: the whole reply.
    let (shapes, _) = frame(vec![egui::Event::PointerGone]);
    let second = rect_of(&shapes, "Second");
    let (a, b) = (second.left_center() + vec2(1.0, 0.0), second.right_center() - vec2(1.0, 0.0));
    frame(vec![egui::Event::PointerMoved(a), press(a, Primary, true)]);
    frame(vec![egui::Event::PointerMoved(b)]);
    frame(vec![press(b, Primary, false)]);
    frame(vec![key(egui::Key::A, egui::Modifiers::COMMAND)]);
    frame(vec![]);
    let (_, copied) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec!["Intro line.\n\n1. First\n2. Second\n   – nested\n\nfn main() {}\n\nOutro line.".to_string()]);
    assert_eq!(crate::select::selected_text(&ctx).as_deref(), Some(markdown::plain_text(reply).as_str()));
    // A right-click keeps the whole-message highlight. Choosing Copy must
    // keep it too, just as the menu keeps an ordinary text selection.
    frame(vec![egui::Event::PointerMoved(b)]);
    let (_, copied) = frame(vec![press(b, Secondary, true), press(b, Secondary, false)]);
    assert!(copied.is_empty(), "opening the menu must not change the clipboard");
    frame(vec![]);
    let (shapes, _) = frame(vec![]);
    let copy = rect_of(&shapes, "Copy").center();
    frame(vec![egui::Event::PointerMoved(copy)]);
    frame(vec![press(copy, Primary, true)]);
    let (_, copied) = frame(vec![press(copy, Primary, false)]);
    assert_eq!(copied, vec![markdown::plain_text(reply)]);
    assert_eq!(crate::select::selected_text(&ctx).as_deref(), Some(markdown::plain_text(reply).as_str()));
    let (_, copied) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec![markdown::plain_text(reply)]);

    // A click in the message ends it. A plain click still establishes a
    // text cursor, so Ctrl+A there works without a preliminary drag too.
    frame(vec![egui::Event::PointerMoved(a)]);
    frame(vec![press(a, Primary, true)]);
    frame(vec![press(a, Primary, false)]);
    frame(vec![]);
    assert_eq!(crate::select::selected_text(&ctx), None);
    frame(vec![key(egui::Key::A, egui::Modifiers::COMMAND)]);
    let (_, copied) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec![markdown::plain_text(reply)]);

    // Focusing a field ends the transcript selection; its Ctrl+A and
    // Copy belong to the field, not the previously selected message.
    let (shapes, _) = frame(vec![]);
    let field = rect_of(&shapes, "draft field text").center();
    frame(vec![egui::Event::PointerMoved(field)]);
    frame(vec![press(field, Primary, true)]);
    frame(vec![press(field, Primary, false)]);
    assert_eq!(crate::select::selected_text(&ctx), None);
    frame(vec![key(egui::Key::A, egui::Modifiers::COMMAND)]);
    let (_, copied) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec!["draft field text".to_string()]);
    assert!(ctx.memory(|m| m.has_focus(field_id)));
}

/// Middle-click scrolling in a transcript: a plain middle click turns it on
/// and the pointer's distance below it scrolls; any click ends it without
/// reaching what is under it, and the text selection stays. Held and
/// dragged, letting go ends it; the wheel ends it; a middle click outside a
/// transcript is the widget's there (a tab's, to close it).
#[test]
fn middle_click_scrolls_a_transcript() {
    let (ctx, t) = test_ctx(true, true);
    copy_menu::install(&ctx);
    autoscroll::install(&ctx);
    let messages: Vec<Value> = (0..60)
        .map(|i| serde_json::json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": [{"type": "text", "text": format!("message number {i}")}]}))
        .collect();
    let conv = Conversation { id: "autoscroll".into(), messages: messages.into(), ..Default::default() };
    let live = Live::default();
    let mut list = vlist::List::default();
    list.follow = false;
    let mut time = 0.0;
    let (tab_middle, tab_clicks) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
    let mut frame = |events: Vec<egui::Event>| {
        let out = run_frame(&ctx, vec2(1000.0, 700.0), &mut time, 1.0 / 60.0, events, |ui| {
            let r = ui.max_rect();
            let (strip, rest) = r.split_top_bottom_at_y(r.top() + 40.0);
            let tab = ui.put(strip, egui::Button::new("the tab"));
            tab_middle.set(tab_middle.get() + tab.middle_clicked() as u32);
            tab_clicks.set(tab_clicks.get() + tab.clicked() as u32);
            ui.scope_builder(egui::UiBuilder::new().max_rect(rest), |ui| chat_view::read_only_view(ui, &t, &conv, &live, &mut list, 0, |_| {}));
        });
        (painted(&out), copied(&out), out.platform_output.cursor_icon)
    };
    let top = |shapes: &[(String, Rect)]| shapes.iter().find(|(s, r)| s.starts_with("message number") && r.top() > 40.0).map(|(s, r)| (s.clone(), r.top())).unwrap();
    for _ in 0..5 {
        frame(vec![]);
    }
    let (shapes, ..) = frame(vec![]);
    assert_eq!(top(&shapes).0, "message number 0");
    // Select "message" in the first message.
    let first = rect_of(&shapes, "message number 0");
    let (from, to) = (egui::pos2(first.left() + 1.0, first.center().y), egui::pos2(first.left() + 52.0, first.center().y));
    frame(vec![egui::Event::PointerMoved(from)]);
    frame(vec![press(from, Primary, true)]);
    frame(vec![egui::Event::PointerMoved(to)]);
    frame(vec![press(to, Primary, false)]);
    let (_, copied, _) = frame(vec![egui::Event::Copy]);
    let selected = copied.first().cloned().expect("a selection");
    assert!(selected.starts_with("messag"), "{selected:?}");

    // A middle click on the selection turns it on without touching it; a
    // left click ends it, and neither reaches the text.
    let at = egui::pos2(500.0, first.center().y + 20.0);
    let (_, _, cursor) = frame(click(at, egui::PointerButton::Middle));
    assert_eq!(cursor, egui::CursorIcon::AllScroll);
    frame(click(at, Primary));
    let (_, copied, cursor) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec![selected.clone()], "the selection stays");
    assert_ne!(cursor, egui::CursorIcon::AllScroll);

    // On again; the pointer 150 points below scrolls down, frame after frame.
    frame(click(at, egui::PointerButton::Middle));
    let below = at + vec2(0.0, 150.0);
    let (_, _, cursor) = frame(vec![egui::Event::PointerMoved(below)]);
    assert_eq!(cursor, egui::CursorIcon::ResizeSouth);
    let mut tops = vec![];
    for _ in 0..10 {
        let (shapes, ..) = frame(vec![]);
        tops.push(top(&shapes));
    }
    assert!(tops.windows(2).all(|w| w[1] != w[0]), "scrolls every frame: {tops:?}");
    assert_ne!(tops[9].0, "message number 0", "{tops:?}");
    // A middle click on the tab ends it, and doesn't close the tab.
    let tab = egui::pos2(500.0, 20.0);
    frame(click(tab, egui::PointerButton::Middle));
    let (shapes, ..) = frame(vec![]);
    let stopped = top(&shapes);
    for _ in 0..3 {
        let (shapes, ..) = frame(vec![egui::Event::PointerMoved(below)]);
        assert_eq!(top(&shapes), stopped, "stopped");
    }
    assert_eq!(tab_middle.get(), 0);
    // Otherwise a middle click on the tab is the tab's.
    frame(click(tab, egui::PointerButton::Middle));
    frame(vec![]);
    assert_eq!(tab_middle.get(), 1);

    // Held and moved up, it scrolls back; letting go ends it.
    frame(vec![egui::Event::PointerMoved(below), press(below, egui::PointerButton::Middle, true)]);
    let above = below - vec2(0.0, 150.0);
    frame(vec![egui::Event::PointerMoved(above)]);
    for _ in 0..5 {
        frame(vec![]);
    }
    let (shapes, ..) = frame(vec![press(above, egui::PointerButton::Middle, false)]);
    let (_, y) = top(&shapes);
    assert!(top(&shapes) != stopped && y.is_finite(), "scrolled back up");
    let (shapes, ..) = frame(vec![]);
    let held = top(&shapes);
    for _ in 0..3 {
        assert_eq!(top(&frame(vec![]).0), held, "letting go ended it");
    }
    // The wheel ends it too (and scrolls as ever).
    frame(click(below, egui::PointerButton::Middle));
    frame(vec![]);
    frame(vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, 1.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Wheel, modifiers: Modifiers::NONE }]);
    for _ in 0..20 {
        frame(vec![]);
    }
    let settled = top(&frame(vec![]).0);
    for _ in 0..3 {
        assert_eq!(top(&frame(vec![]).0), settled, "the wheel ended it");
    }
    assert_eq!(tab_clicks.get(), 0);
}

/// Middle-click scrolling in a chat drawn as widgets (a plugin's): from
/// the bottom it is kept at, the pointer above the ring
/// scrolls up and stays there, then below scrolls back down; the click that
/// ends it leaves the text selection alone.
#[test]
fn middle_click_scrolls_a_widget_chat() {
    let (ctx, t) = test_ctx(true, true);
    copy_menu::install(&ctx);
    autoscroll::install(&ctx);
    let mut time = 0.0;
    let mut frame = |events: Vec<egui::Event>| {
        let out = run_frame(&ctx, vec2(1000.0, 700.0), &mut time, 1.0 / 60.0, events, |ui| {
            composer::chat_scroll(ui, &t, "widget-chat", 6.0, |ui| {
                for i in 0..80 {
                    ui.label(format!("chat line {i}"));
                }
            });
        });
        (painted(&out), copied(&out), out.platform_output.cursor_icon)
    };
    let last = |shapes: &[(String, Rect)]| {
        shapes.iter().filter(|(s, r)| s.starts_with("chat line") && r.bottom() < 700.0).map(|(s, _)| s[10..].parse::<u32>().unwrap()).max().unwrap()
    };
    for _ in 0..5 {
        frame(vec![]);
    }
    let (shapes, ..) = frame(vec![]);
    assert_eq!(last(&shapes), 79, "kept at the bottom");
    // Select "chat" in the last line.
    let line = rect_of(&shapes, "chat line 79");
    let (from, to) = (egui::pos2(line.left() + 1.0, line.center().y), egui::pos2(line.left() + 26.0, line.center().y));
    frame(vec![egui::Event::PointerMoved(from)]);
    frame(vec![press(from, Primary, true)]);
    frame(vec![egui::Event::PointerMoved(to)]);
    frame(vec![press(to, Primary, false)]);
    let (_, copied, _) = frame(vec![egui::Event::Copy]);
    let selected = copied.first().cloned().expect("a selection");
    assert!(selected.starts_with("cha"), "{selected:?}");

    // A middle click turns it on and a left click off, neither touching it.
    let at = egui::pos2(500.0, 400.0);
    frame(click(line.center(), egui::PointerButton::Middle));
    frame(click(line.center(), Primary));
    let (_, copied, _) = frame(vec![egui::Event::Copy]);
    assert_eq!(copied, vec![selected.clone()], "the selection stays");

    // On, with the pointer 150 points above: it leaves the bottom, every frame.
    let (_, _, cursor) = frame(click(at, egui::PointerButton::Middle));
    assert_eq!(cursor, egui::CursorIcon::AllScroll);
    let above = at - vec2(0.0, 150.0);
    let (_, _, cursor) = frame(vec![egui::Event::PointerMoved(above)]);
    assert_eq!(cursor, egui::CursorIcon::ResizeNorth);
    let mut lasts = vec![];
    for _ in 0..12 {
        lasts.push(last(&frame(vec![]).0));
    }
    assert!(lasts[11] < 75, "scrolled up: {lasts:?}");
    assert!(lasts.windows(2).all(|w| w[1] <= w[0]), "{lasts:?}");
    // A left click ends it where it is, reaching nothing.
    frame(click(above, Primary));
    let stopped = last(&frame(vec![]).0);
    for _ in 0..5 {
        assert_eq!(last(&frame(vec![]).0), stopped, "stays up");
    }
    // Held and dragged down, it scrolls back down; letting go ends it.
    frame(vec![egui::Event::PointerMoved(at), press(at, egui::PointerButton::Middle, true)]);
    let below = at + vec2(0.0, 150.0);
    frame(vec![egui::Event::PointerMoved(below)]);
    for _ in 0..8 {
        frame(vec![]);
    }
    frame(vec![press(below, egui::PointerButton::Middle, false)]);
    let down = last(&frame(vec![]).0);
    assert!(down > stopped, "scrolled down: {stopped} to {down}");
    for _ in 0..3 {
        assert_eq!(last(&frame(vec![]).0), down, "letting go ended it");
    }
}

