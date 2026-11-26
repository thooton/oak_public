//! Asks Quick what it thinks of this machine's outpost: registered, and
//! whether it is ONLINE (which is what makes it eligible to run tools).
//!
//! OAK_DIR=<profile> cargo test --lib quick_outpost_status -- --ignored --nocapture

#[test]
#[ignore]
fn quick_outpost_status() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR");
    crate::api::load_auth();
    let host = crate::quick::login::host().expect("host");
    let token = crate::quick::login::access_token().expect("token");
    println!("outpost id Oak uses: {}", crate::quick::outpost::id());
    println!("registering: {:?}", crate::quick::outpost::register());

    let body = serde_json::json!({
        "eventType": "listOutposts",
        "eventBody": {"listOutposts": {"scope": "USER", "surface": "QUICK_WORK"}}
    });
    let resp = crate::api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "outpost")
        .header("x-amzn-quick-atype", "sbg")
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .expect("listOutposts");
    let text = resp.text().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if items_empty(&v) {
        println!(
            "\nRAW (first 900 chars):\n{}",
            text.chars().take(900).collect::<String>()
        );
    }
    let items = v["eventResponse"]["listOutpostsResponse"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // The folders Quick has for this device.
    let fbody = serde_json::json!({
        "eventType": "invokeAgent",
        "eventBody": {"invokeAgent": {
            "path": "graph/file-sync-folders", "queryParams": {},
            "body": format!("{{\"endpoint_id\": \"probe\", \"device_id\": \"{}\"}}", crate::quick::outpost::id()),
            "outpostId": crate::quick::outpost::id()
        }}
    });
    if let Ok(r) = crate::api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .json(&fbody)
        .timeout(std::time::Duration::from_secs(30))
        .send()
    {
        let t = r.text().unwrap_or_default();
        let k = t.find("invokeAgentResponse").unwrap_or(0);
        let end = (k + 700).min(t.len());
        println!("\nfolders answer: {}", &t[k..end]);
    }

    // What graph/status says for Oak's own outpost once it is told.
    println!(
        "\nexpected {}, online now: {:?}",
        crate::quick::outpost::id(),
        crate::quick::outpost::is_online(&token, &host)
    );
    println!("brute-forcing the device-state field:");
    for (field, accepted, note) in crate::quick::outpost::put_device_state_report(&token, &host) {
        let now = crate::quick::outpost::is_online(&token, &host).unwrap_or(false);
        println!(
            "  {field:18} accepted={accepted:5} online={now}{}",
            if accepted { "" } else { "  <- rejected" }
        );
        if now {
            println!("\n*** {field} brings the outpost ONLINE ***");
            break;
        }
        let _ = note;
    }

    println!("\n{} outposts on this account:", items.len());
    for o in &items {
        println!(
            "  - id={} name={} status={} type={}",
            o["id"].as_str().unwrap_or("?"),
            o["name"].as_str().unwrap_or("?"),
            o["status"].as_str().unwrap_or("?"),
            o["outpostType"].as_str().unwrap_or("?")
        );
    }
}

fn items_empty(v: &serde_json::Value) -> bool {
    v["eventResponse"]["listOutpostsResponse"]["items"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

/// Holds this machine's outpost ONLINE: a chat socket that names it, kept open
/// (and reopened when it drops) for as long as the process runs. Quick lists an
/// outpost as ONLINE only while a client that names it is connected, so this is
/// what makes this machine visible to Quick while Oak isn't in a turn.
///
/// OAK_DIR=<profile> OAK_QUICK_KEEP_MINUTES=240 \
///   cargo test --lib quick_outpost_stay_online -- --ignored --nocapture
#[test]
#[ignore]
fn quick_outpost_stay_online() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR");
    crate::api::load_auth();
    let host = crate::quick::login::host().expect("host");
    let token = crate::quick::login::access_token().expect("token");
    let outpost = crate::quick::outpost::id();
    let minutes: u64 = std::env::var("OAK_QUICK_KEEP_MINUTES")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(240);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(minutes * 60);
    println!("holding outpost {outpost} ONLINE for up to {minutes} minutes");
    println!("register: {:?}", crate::quick::outpost::register());
    // A watcher, so the log says whether Quick still lists it ONLINE.
    {
        let (host, token, outpost) = (host.clone(), token.clone(), outpost.clone());
        std::thread::spawn(move || {
            loop {
                println!(
                    "[{}] listOutposts says {outpost} {}",
                    clock(),
                    status_of(&token, &host, &outpost)
                );
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        });
    }
    while std::time::Instant::now() < end {
        let path = format!(
            "/streaming/quickwork-chat-bidirectional-stream?surfaceType={}&token={}&outpostId={outpost}&workingSessionId=oak-keep-{}&channel=prod",
            crate::quick::SURFACE,
            enc(&token),
            crate::api::now_ms(),
        );
        let mut sock = match crate::quick::socket::Socket::connect(&host, &path) {
            Ok(s) => s,
            Err(e) => {
                println!("[{}] connect failed: {e}", clock());
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        let base = serde_json::json!({
            "surfaceType": crate::quick::SURFACE,
            "appVersion": crate::quick::APP_VERSION,
            "acp_first_text": 1,
            "user_notification": 1,
            "subtask_progress": 1,
            "external_agent_sign_in": 1,
            "recents_promotion": 1,
            "outpostId": outpost,
        });
        let mut connect = base.clone();
        connect["actor_id"] = serde_json::json!("");
        let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": connect}));
        let _ = sock.send(&serde_json::json!({"type": "federate_status", "payload": {}}));
        let mut home = base.clone();
        home["session_id"] = serde_json::json!("home");
        home["focus_only"] = serde_json::json!(true);
        let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": home}));
        let _ = sock.send(
            &serde_json::json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}}),
        );
        println!("[{}] socket open, outpost named; waiting on Quick", clock());
        // Nothing is done with Quick's frames: any tool it asks for is another
        // client's to run. Reading them is what keeps the socket alive.
        while std::time::Instant::now() < end {
            match sock.next() {
                Some(v) => {
                    let kind = v["type"].as_str().unwrap_or("");
                    if kind == "tool_call" {
                        println!(
                            "[{}] Quick asked for a tool ({}); not run by the keeper",
                            clock(),
                            v["payload"]["tool_name"].as_str().unwrap_or("?")
                        );
                    }
                }
                None => break,
            }
        }
        println!("[{}] socket closed", clock());
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// The time, for a log line.
fn clock() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

/// Does a chat socket that names the outpost in its URL make Quick list that
/// outpost ONLINE? Measured at every step, on an outpost of its own (never the
/// machine's, which Quick's app may be keeping alive), so the answer is the
/// server's, not a guess.
///
/// OAK_DIR=<profile> cargo test --lib quick_outpost_ws_online -- --ignored --nocapture
#[test]
#[ignore]
fn quick_outpost_ws_online() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR");
    crate::api::load_auth();
    let host = crate::quick::login::host().expect("host");
    let token = crate::quick::login::access_token().expect("token");
    let probe = format!("oak-probe-{}", crate::api::now_ms());
    println!("probe outpost: {probe}");
    println!("register: {:?}", crate::quick::outpost::register_as(&probe));
    let outcome = measure_online(&token, &host, &probe);
    println!("\n{outcome}");
    println!("unregister: {:?}", unregister(&token, &host, &probe));
}

/// Where along the connect sequence the probe turns ONLINE; says so in words.
fn measure_online(token: &str, host: &str, probe: &str) -> String {
    let status = |label: &str| -> String {
        let mut last = String::new();
        for i in 0..5 {
            last = status_of(token, host, probe);
            println!("  [{label}] {i}: {last}");
            if last == "ONLINE" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        last
    };
    if status("before any socket") == "ONLINE" {
        return "ONLINE before the socket: the probe is no good".into();
    }
    let path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={}&token={}&outpostId={probe}&workingSessionId=oak-live-{}&channel=prod",
        crate::quick::SURFACE,
        enc(token),
        crate::api::now_ms(),
    );
    println!("\nconnecting with the outpost named in the URL");
    let mut sock = match crate::quick::socket::Socket::connect(host, &path) {
        Ok(s) => s,
        Err(e) => return format!("connect failed: {e}"),
    };
    let base = serde_json::json!({
        "surfaceType": crate::quick::SURFACE,
        "appVersion": crate::quick::APP_VERSION,
        "acp_first_text": 1,
        "user_notification": 1,
        "subtask_progress": 1,
        "external_agent_sign_in": 1,
        "recents_promotion": 1,
        "outpostId": probe,
    });
    let mut connect = base.clone();
    connect["actor_id"] = serde_json::json!("");
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": connect}));
    if status("after session_connect") == "ONLINE" {
        return "ONLINE: the socket's outpost and its session_connect did it".into();
    }
    let _ = sock.send(&serde_json::json!({"type": "federate_status", "payload": {}}));
    if status("after federate_status") == "ONLINE" {
        return "ONLINE after federate_status".into();
    }
    let mut home = base.clone();
    home["session_id"] = serde_json::json!("home");
    home["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": home}));
    if status("after home") == "ONLINE" {
        return "ONLINE once home is joined".into();
    }
    let mut focus = base.clone();
    focus["session_id"] = serde_json::json!(format!("oak-live-{}", crate::api::now_ms()));
    focus["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": focus}));
    if status("after thread") == "ONLINE" {
        return "ONLINE once the thread is joined".into();
    }
    drop(sock);
    format!(
        "never ONLINE; after the socket closed it is {}",
        status_of(token, host, probe)
    )
}

/// Several chat sockets naming one outpost, as parallel Quick conversations
/// on one machine open them (one per turn): is the outpost ONLINE while any is
/// connected, or does one closing take it OFFLINE under the others? And the
/// same across two accounts that registered the same outpost id. Measured on
/// a probe outpost of its own, so no live conversation's outpost is touched.
///
/// OAK_DIR=<copy of a profile> OAK_QUICK_LOGIN_A=<id> OAK_QUICK_LOGIN_B=<id> \
///   cargo test --lib quick_outpost_shared_sockets -- --ignored --nocapture
#[test]
#[ignore]
fn quick_outpost_shared_sockets() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR");
    crate::api::load_auth();
    let a = std::env::var("OAK_QUICK_LOGIN_A").expect("OAK_QUICK_LOGIN_A");
    let b = std::env::var("OAK_QUICK_LOGIN_B").ok();
    let probe = format!("oak-probe-{}", crate::api::now_ms());
    let as_login = |id: &str| {
        crate::quick::login::set_using(Some(id.to_string()));
        (crate::quick::login::access_token().expect("token"), crate::quick::login::host().expect("host"))
    };
    let (ta, host) = as_login(&a);
    println!("probe {probe} on {a}: register {:?}", crate::quick::outpost::register_as(&probe));
    let watch = |label: &str, secs: u64| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut seen = vec![];
        while std::time::Instant::now() < end {
            let s = status_of(&ta, &host, &probe);
            if seen.last() != Some(&s) {
                println!("  [{}] {label}: {s}", clock());
                seen.push(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
        seen.last().cloned().unwrap_or_default()
    };
    watch("before any socket", 3);
    let s1 = open_named(&ta, &host, &probe);
    let one = watch("socket 1 open", 8);
    let s2 = open_named(&ta, &host, &probe);
    let two = watch("sockets 1 and 2 open", 8);
    drop(s2);
    let after2 = watch("socket 2 closed, socket 1 still open", 20);
    drop(s1);
    let none = watch("both closed", 10);
    println!("SAME ACCOUNT: one open {one}, two open {two}, one of two closed {after2}, none {none}");
    // Order, and a keeper: the earlier of two closing; a keeper (no thread
    // joined) opened beside a turn's; a keeper replaced, old one closed first.
    let s1 = open_named(&ta, &host, &probe);
    watch("s1 open", 6);
    let s2 = open_named(&ta, &host, &probe);
    watch("s1, s2 open", 5);
    drop(s1);
    let early = watch("earlier s1 closed, s2 open", 12);
    let k1 = open_keeper(&ta, &host, &probe);
    let kept = watch("keeper opened beside s2", 10);
    let s3 = open_named(&ta, &host, &probe);
    watch("s3 opened too", 5);
    drop(s3);
    watch("s3 closed (s2, keeper open)", 10);
    drop(k1);
    let k2 = open_keeper(&ta, &host, &probe);
    let renewed = watch("keeper replaced: old closed, new opened", 15);
    drop(s2);
    drop(k2);
    println!("ORDER: earlier closed {early}, keeper after {kept}, keeper renewed after a close {renewed}");
    if let Some(b) = b.filter(|b| *b != a) {
        let s1 = open_named(&ta, &host, &probe);
        watch("socket 1 (A) open again", 8);
        let (tb, host_b) = as_login(&b);
        println!("same probe on {b}: register {:?}", crate::quick::outpost::register_as(&probe));
        println!("  B's list says {}", status_of(&tb, &host_b, &probe));
        let s3 = open_named(&tb, &host_b, &probe);
        let both = watch("A's socket 1 and B's socket open (A's view)", 8);
        println!("  B's list says {}", status_of(&tb, &host_b, &probe));
        drop(s3);
        let after = watch("B's socket closed, A's open (A's view)", 20);
        drop(s1);
        println!("TWO ACCOUNTS: both open {both}, B's closed {after}");
        println!("unregister on B: {:?}", unregister(&tb, &host_b, &probe));
        crate::quick::login::set_using(Some(a.clone()));
    }
    println!("unregister on A: {:?}", unregister(&ta, &host, &probe));
}

/// Oak's keeper (`keeper`) at work, on a probe outpost: two turns' sockets
/// open, the newer closes, and the outpost should stay ONLINE (the keeper
/// takes over) until the last one closes.
///
/// OAK_DIR=<copy of a profile> OAK_QUICK_LOGIN_A=<id> OAK_QUICK_KEEPER=1 \
///   cargo test --lib quick_outpost_keeper_holds -- --ignored --nocapture
#[test]
#[ignore]
fn quick_outpost_keeper_holds() {
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR");
    assert!(std::env::var_os("OAK_QUICK_KEEPER").is_some(), "set OAK_QUICK_KEEPER=1");
    crate::api::load_auth();
    let a = std::env::var("OAK_QUICK_LOGIN_A").expect("OAK_QUICK_LOGIN_A");
    crate::quick::login::set_using(Some(a.clone()));
    // (The vault may still be connecting.)
    let token = (0..40)
        .find_map(|_| crate::quick::login::access_token().ok().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            None
        }))
        .expect("token");
    let host = crate::quick::login::host().expect("host");
    let probe = format!("oak-probe-{}", crate::api::now_ms());
    println!("probe {probe} on {a}: register {:?}", crate::quick::outpost::register_as(&probe));
    let watch = |label: &str, secs: u64| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut seen: Vec<String> = vec![];
        while std::time::Instant::now() < end {
            let s = status_of(&token, &host, &probe);
            if seen.last() != Some(&s) {
                println!("  [{}] {label}: {s}", clock());
                seen.push(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
        seen
    };
    let s1 = open_named(&token, &host, &probe);
    watch("turn 1 open", 6);
    let s2 = open_named(&token, &host, &probe);
    watch("turns 1 and 2 open", 5);
    drop(s2);
    let held = watch("turn 2 closed, turn 1 open (keeper)", 25);
    let s3 = open_named(&token, &host, &probe);
    watch("turn 3 opened", 5);
    drop(s3);
    let again = watch("turn 3 closed, turn 1 open (keeper replaced)", 25);
    drop(s1);
    let gone = watch("all turns closed (keeper closes)", 15);
    println!("KEEPER: after a close {held:?}, after another {again:?}, none open {gone:?}");
    println!("unregister: {:?}", unregister(&token, &host, &probe).map(|s| s.len()));
}

/// Where Quick delivers a device call while a keeper is the newest socket
/// naming the outpost: an older turn socket on a probe thread, then a keeper,
/// then the thread asks for a device tool. Prints which socket each
/// `tool_call` came on, answering it there so the turn ends.
///
/// OAK_DIR=<copy of a profile> OAK_QUICK_LOGIN_A=<id> \
///   cargo test --lib quick_outpost_calls_reach_the_turn -- --ignored --nocapture
#[test]
#[ignore]
fn quick_outpost_calls_reach_the_turn() {
    use crate::quick::socket::PollFrame;
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR");
    crate::api::load_auth();
    let a = std::env::var("OAK_QUICK_LOGIN_A").expect("OAK_QUICK_LOGIN_A");
    crate::quick::login::set_using(Some(a.clone()));
    let token = (0..40)
        .find_map(|_| crate::quick::login::access_token().ok().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            None
        }))
        .expect("token");
    let host = crate::quick::login::host().expect("host");
    let probe = format!("oak-probe-{}", crate::api::now_ms());
    println!("probe {probe} on {a}: register {:?}", crate::quick::outpost::register_as(&probe));
    let thread = crate::quick::hex_uuid();
    let mut turn = open_named_on(&token, &host, &probe, &thread);
    let _ = turn.send(&serde_json::json!({"type": "dock_state_sync", "payload": {"session_id": thread, "files": [], "focused_file": "", "sidebar_open": false, "surfaceType": crate::quick::SURFACE}}));
    std::thread::sleep(std::time::Duration::from_secs(3));
    let mut keeper = open_keeper(&token, &host, &probe);
    for _ in 0..20 {
        if status_of(&token, &host, &probe) == "ONLINE" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    println!("[{}] turn socket, then keeper open: {}", clock(), status_of(&token, &host, &probe));
    let text = "Call list_outposts, then call folder_list on the ONLINE device (no path), once each. Then reply with the single word done.";
    let user = serde_json::json!({"type": "user_message", "payload": {
        "traceId": crate::quick::hex_uuid(), "text": text, "send_origin": "composer", "had_physical_input": true,
        "surfaceType": crate::quick::SURFACE, "appVersion": crate::quick::APP_VERSION, "activityType": "chat",
        "surface": "desktop", "clientVersion": crate::quick::APP_VERSION, "cohort": "general_availability",
        "message_id": format!("msg-{}", &crate::quick::hex_uuid().replace('-', "")[..8]), "thread_id": thread,
        "agent_mode": "fast", "thinking_effort": "low", "user_timezone": "America/Phoenix", "user_locale": "en",
        "chat_agent_id": crate::quick::CHAT_AGENT, "auto_routing": false, "web_search_enabled": false,
        "direct_chat": false, "voice_mode": false, "outpostId": probe,
    }});
    println!("send: {:?}", turn.send(&user));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(150);
    let (mut on_turn, mut on_keeper, mut done) = (0, 0, false);
    while std::time::Instant::now() < end && !done {
        for (who, sock) in [("TURN", &mut turn), ("KEEPER", &mut keeper)] {
            let PollFrame::Frame(f) = sock.poll_next(std::time::Duration::from_millis(150)) else { continue };
            let kind = f["type"].as_str().unwrap_or("").to_string();
            let p = &f["payload"];
            match kind.as_str() {
                "tool_call" => {
                    let name = p["tool_name"].as_str().unwrap_or("?");
                    let id = p["tool_call_id"].as_str().unwrap_or("");
                    println!("[{}] tool_call {name} ({id}) came on the {who} socket, for thread {:?}", clock(), crate::quick::turn::call_thread(p));
                    if who == "TURN" { on_turn += 1 } else { on_keeper += 1 }
                    // As Oak's keeper does: it answers nothing.
                    if who == "KEEPER" && std::env::var_os("OAK_PROBE_KEEPER_SILENT").is_some() {
                        continue;
                    }
                    let reply = crate::quick::turn::tool_reply(id, &Ok("{\"success\": true, \"entries\": [\"probe.txt\"]}".into()), "");
                    let _ = sock.send(&reply);
                }
                "tool_start" => println!("[{}] tool_start {} on {who}", clock(), p["tool_name"].as_str().unwrap_or("?")),
                "tool_result" => println!("[{}] tool_result on {who}: {}", clock(), p.to_string().chars().take(160).collect::<String>()),
                "message_complete" | "error" => {
                    println!("[{}] {kind} on {who}: {}", clock(), p.to_string().chars().take(200).collect::<String>());
                    done = done || who == "TURN";
                }
                _ => {}
            }
        }
    }
    println!("CALLS: on the turn socket {on_turn}, on the keeper {on_keeper}, ended {done}");
    drop(keeper);
    drop(turn);
    println!("unregister: {:?}", unregister(&token, &host, &probe).map(|s| s.len()));
}

/// `open_named` on thread `thread`.
fn open_named_on(token: &str, host: &str, outpost: &str, thread: &str) -> crate::quick::socket::Socket {
    let path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={}&token={}&outpostId={outpost}&workingSessionId={thread}&channel=prod",
        crate::quick::SURFACE,
        enc(token),
    );
    let mut sock = crate::quick::socket::Socket::connect(host, &path).expect("connect");
    let base = serde_json::json!({
        "surfaceType": crate::quick::SURFACE, "appVersion": crate::quick::APP_VERSION, "acp_first_text": 1,
        "user_notification": 1, "subtask_progress": 1, "external_agent_sign_in": 1, "recents_promotion": 1,
        "outpostId": outpost,
    });
    let mut connect = base.clone();
    connect["actor_id"] = serde_json::json!("");
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": connect}));
    let _ = sock.send(&serde_json::json!({"type": "federate_status", "payload": {}}));
    let mut home = base.clone();
    home["session_id"] = serde_json::json!("home");
    home["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": home}));
    let _ = sock.send(&serde_json::json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}}));
    let mut focus = base;
    focus["session_id"] = serde_json::json!(thread);
    focus["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": focus}));
    sock
}

/// A socket that only names `outpost` (its session and home, no thread), as
/// a keeper holding it ONLINE would.
fn open_keeper(token: &str, host: &str, outpost: &str) -> crate::quick::socket::Socket {
    let path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={}&token={}&outpostId={outpost}&workingSessionId=oak-keep-{}&channel=prod",
        crate::quick::SURFACE,
        enc(token),
        crate::api::now_ms(),
    );
    let mut sock = crate::quick::socket::Socket::connect(host, &path).expect("connect");
    let base = serde_json::json!({"surfaceType": crate::quick::SURFACE, "appVersion": crate::quick::APP_VERSION, "outpostId": outpost});
    let mut connect = base.clone();
    connect["actor_id"] = serde_json::json!("");
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": connect}));
    let _ = sock.send(&serde_json::json!({"type": "federate_status", "payload": {}}));
    let mut home = base;
    home["session_id"] = serde_json::json!("home");
    home["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": home}));
    let _ = sock.send(&serde_json::json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}}));
    sock
}

/// A chat socket naming `outpost`, connected as a turn connects one (its
/// session, home and a thread of its own), but with nothing sent in it.
fn open_named(token: &str, host: &str, outpost: &str) -> crate::quick::socket::Socket {
    let thread = format!("oak-probe-thread-{}", crate::api::now_ms());
    let path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={}&token={}&outpostId={outpost}&workingSessionId={thread}&channel=prod",
        crate::quick::SURFACE,
        enc(token),
    );
    let mut sock = crate::quick::socket::Socket::connect(host, &path).expect("connect");
    let base = serde_json::json!({
        "surfaceType": crate::quick::SURFACE,
        "appVersion": crate::quick::APP_VERSION,
        "acp_first_text": 1,
        "user_notification": 1,
        "subtask_progress": 1,
        "external_agent_sign_in": 1,
        "recents_promotion": 1,
        "outpostId": outpost,
    });
    let mut connect = base.clone();
    connect["actor_id"] = serde_json::json!("");
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": connect}));
    let _ = sock.send(&serde_json::json!({"type": "federate_status", "payload": {}}));
    let mut home = base.clone();
    home["session_id"] = serde_json::json!("home");
    home["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": home}));
    let _ = sock.send(&serde_json::json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}}));
    let mut focus = base;
    focus["session_id"] = serde_json::json!(thread);
    focus["focus_only"] = serde_json::json!(true);
    let _ = sock.send(&serde_json::json!({"type": "session_connect", "payload": focus}));
    sock
}

/// Removes a probe outpost from the account, so the list stays the user's.
fn unregister(token: &str, host: &str, outpost: &str) -> Result<String, String> {
    let body = serde_json::json!({"eventType": "unregisterOutpost", "eventBody": {"unregisterOutpost": {"outpostId": outpost}}});
    let r = crate::api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "outpost")
        .header("x-amzn-quick-atype", "sbg")
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .map_err(crate::api::network_error)?;
    Ok(r.text().unwrap_or_default().chars().take(160).collect())
}

/// The status `listOutposts` gives `outpost` ("OFFLINE", "ONLINE", or "-"
/// when Quick doesn't list it).
fn status_of(token: &str, host: &str, outpost: &str) -> String {
    let body = serde_json::json!({
        "eventType": "listOutposts",
        "eventBody": {"listOutposts": {"scope": "USER", "surface": "QUICK_WORK"}}
    });
    let Ok(r) = crate::api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "outpost")
        .header("x-amzn-quick-atype", "sbg")
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
    else {
        return "?".into();
    };
    let v: serde_json::Value =
        serde_json::from_str(&r.text().unwrap_or_default()).unwrap_or(serde_json::Value::Null);
    v["eventResponse"]["listOutpostsResponse"]["items"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["id"].as_str() == Some(outpost)))
        .and_then(|o| o["status"].as_str())
        .map(String::from)
        .unwrap_or_else(|| "-".into())
}

/// Percent-encodes a token for a URL query, as the socket's address needs it.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
