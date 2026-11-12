// Carrying a conversation to another account. When an account reaches its
// usage limit, the whole event history is written out as text, stored on the
// provider's kit's server (POST /conversations), and the new session's first
// message opens with a block (the kit's carry tags) naming it for the kit's
// tool to `resume`; pictures ride along on that message.
use super::*;

/// Every event payload of a session, oldest first.
pub(crate) fn history(rid: &str, id: &str) -> Result<Vec<Value>, String> {
    let mut payloads = vec![];
    let mut cursor: Option<String> = None;
    for _ in 0..10_000 {
        let mut path = format!("/v1/code/sessions/{}/events?sort_order=asc", cse(id));
        if let Some(c) = &cursor {
            path += &format!("&cursor={}", crate::web::percent(c));
        }
        let listed = call(rid, reqwest::Method::GET, &path, false, true, None)?;
        let data = listed["data"].as_array().cloned().unwrap_or_default();
        for e in &data {
            if let Some(n) = e["sequence_num"].as_u64() {
                cursor = Some(n.to_string());
            } else if let Some(s) = e["sequence_num"].as_str() {
                cursor = Some(s.to_string());
            }
            if e["payload"].is_object() {
                payloads.push(e["payload"].clone());
            }
        }
        if data.is_empty() || listed["next_cursor"].is_null() || listed["next_cursor"] == "" {
            return Ok(payloads);
        }
    }
    Err("The session's history is too long to read.".into())
}

/// Nothing quoted may close the block early.
fn carry_clean(text: &str) -> String {
    let mut text = text.to_string();
    for p in Provider::all() {
        let close = p.tags().carry.1;
        if let Some(inner) = close.strip_suffix('>') {
            text = text.replace(&close, &format!("{inner} >"));
        }
    }
    text
}

/// The whole conversation as text, for `oak resume`, and the pictures the
/// user sent (as image blocks).
pub(crate) fn carry_text(title: &str, events: &[Value]) -> (String, Vec<Value>) {
    let mut out = String::new();
    let mut pictures = vec![];
    let mut tools: HashMap<String, String> = HashMap::new();
    let mut seen = HashSet::new();
    let said = |out: &mut String, who: &str, text: &str| {
        if !text.trim().is_empty() {
            out.push_str(&format!("[{who}]\n{}\n\n", carry_clean(text.trim())));
        }
    };
    let result = |c: &Value| -> String {
        match c {
            Value::Null => "(no output)".into(),
            Value::String(s) if s.is_empty() => "(no output)".into(),
            Value::String(s) => s.clone(),
            Value::Array(a) => {
                let t = a
                    .iter()
                    .filter(|b| b.is_object())
                    .map(|b| match b["type"].as_str() {
                        Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                        Some("image") => "[an image, which can't be carried over; produce it again if you need it]".into(),
                        _ => b.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if t.is_empty() { "(no output)".into() } else { t }
            }
            other => other.to_string(),
        }
    };
    // Any provider's setup block goes: the new session gets its own.
    let setups: Vec<(String, String)> = Provider::all().into_iter().map(|p| p.tags().setup).collect();
    let unsetup = |t: &str| setups.iter().find_map(|(open, close)| strip_block(t, open, close)).unwrap_or_else(|| t.to_string());
    for p in events {
        if !p["parent_tool_use_id"].is_null() {
            continue;
        }
        if let Some(u) = p["uuid"].as_str()
            && !seen.insert(u.to_string())
        {
            continue;
        }
        let content = &p["message"]["content"];
        match p["type"].as_str() {
            Some("user") if p["isMeta"] != true => {
                if let Some(s) = content.as_str() {
                    said(&mut out, "User", &unsetup(s));
                    continue;
                }
                let mut words = vec![];
                for b in content.as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("tool_result") => {
                            let name = tools.get(b["tool_use_id"].as_str().unwrap_or("")).cloned().unwrap_or("a tool call".into());
                            let err = if b["is_error"] == true { " (error)" } else { "" };
                            said(&mut out, &format!("Result of {name}{err}"), &result(&b["content"]));
                        }
                        Some("text") => words.push(unsetup(b["text"].as_str().unwrap_or(""))),
                        Some("image") if b["source"]["type"] == "base64" => {
                            pictures.push(crate::oak::from_anthropic_block(b, "claude"));
                            words.push(format!("[picture {}, attached to the first message]", pictures.len()));
                        }
                        Some("image") => words.push("[a picture, which can't be carried over]".into()),
                        _ => {}
                    }
                }
                if !words.is_empty() {
                    said(&mut out, "User", &words.join("\n\n"));
                }
            }
            Some("assistant") => {
                for b in content.as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => said(&mut out, "You", b["text"].as_str().unwrap_or("")),
                        Some("thinking") => said(&mut out, "You (thinking)", b["thinking"].as_str().unwrap_or("")),
                        Some("tool_use") => {
                            let name = b["name"].as_str().unwrap_or("a tool");
                            if let Some(id) = b["id"].as_str() {
                                tools.insert(id.into(), name.into());
                            }
                            said(&mut out, &format!("You called {name}"), &b["input"].to_string());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let name = title.trim().replace('\n', " ");
    let name = if name.is_empty() { "Untitled".to_string() } else { name };
    let text = format!(
        "# {name}\n\nThe complete conversation of an earlier Oak cloud session, which stopped because the account it ran on \
         reached its usage limit: every message, every tool call with its full input, and every tool result in full, oldest \
         first. \"You\" is the agent. Pictures the user sent are attached to the first message of the session that continues \
         it, in the order they are mentioned.\n\n{out}[End of the conversation. Carry on from here.]\n"
    );
    (text, pictures)
}

/// The block that opens the new session's first message.
pub(crate) fn carry_block(provider: Provider, title: &str, id: &str) -> String {
    let (carry_open, carry_close) = provider.tags().carry;
    let (cli, server) = provider.kit().map(|k| (k.cli(), k.server_name())).unwrap_or_else(|| ("oak".into(), "the server".into()));
    let t = title.trim();
    format!(
        "{carry_open}\nThis conversation continues an earlier session{} that stopped because the account it ran on reached its \
         usage limit; this session runs on another account. The whole conversation so far, every message, tool call and tool \
         result in full, is on {server}. Once the {cli} CLI is set up, and before anything else, run\n\n    \
         {cli} resume {id} conversation.md\n\nwhich saves it to conversation.md, and read ALL of that file, from the first line \
         to the last, in parts if need be. Pictures the user sent earlier are attached to this message, in the order the \
         conversation mentions them. The old session's sandbox did not come along: clone repositories again and redo any work \
         that was not pushed or saved elsewhere. Then pick up exactly where it left off. If the user wrote anything new, it \
         follows this block.\n{carry_close}",
        if t.is_empty() { String::new() } else { format!(" (\"{}\")", carry_clean(t)) }
    )
}
