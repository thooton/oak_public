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
