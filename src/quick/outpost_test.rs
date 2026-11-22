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
    if items_empty(&v) { println!("\nRAW (first 900 chars):\n{}", &text.chars().take(900).collect::<String>()); }
    let items = v["eventResponse"]["listOutpostsResponse"]["items"].as_array().cloned().unwrap_or_default();
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
    println!("\nexpected {}, online now: {:?}", crate::quick::outpost::id(), crate::quick::outpost::is_online(&token, &host));
    println!("brute-forcing the device-state field:");
    for (field, accepted, note) in crate::quick::outpost::put_device_state_report(&token, &host) {
        let now = crate::quick::outpost::is_online(&token, &host).unwrap_or(false);
        println!("  {field:18} accepted={accepted:5} online={now}{}", if accepted { "" } else { "  <- rejected" });
        if now {
            println!("\n*** {field} brings the outpost ONLINE ***");
            break;
        }
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
    v["eventResponse"]["listOutpostsResponse"]["items"].as_array().map(|a| a.is_empty()).unwrap_or(true)
}
