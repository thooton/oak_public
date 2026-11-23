//! Antigravity wire protocol: converts Oak requests to Cloud Code Assist's
//! `v1internal:streamGenerateContent?alt=sse` format and translates SSE chunks
//! back into Oak events, with endpoint failover, sibling-tier fallback,
//! capacity backoff, and multi-account rotation.

use super::{auth, catalog, endpoint_candidates, env_var, error_text, stable_uuid, with_headers};
use crate::api::{self, Sse};
use crate::oak::{self, Event, Request};
use crate::provider;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const CONTINUATION_TEXT: &str = "Continue the active task using the available instructions and context.";
const PLACEHOLDER_SYSTEM_TURN_TEXT: &str = "Apply the active system instructions.";

/// Consumer endpoints return a generic 429 RESOURCE_EXHAUSTED when
/// `systemInstruction` exceeds 126 characters (regardless of wording or part
/// splitting), while accepting the same text inside `contents` at any length.
const SYSTEM_INSTRUCTION_PART_LIMIT: usize = 126;

const MAX_SCHEMA_DEREF_DEPTH: usize = 64;
const MAX_SCHEMA_DEREF_NODES: usize = 10_000;

/// Capacity backoff ladder (ms) for generic 429 RESOURCE_EXHAUSTED (no reset
/// hint) and 503 No capacity available, matching pi-antigravity.
const CAPACITY_BACKOFF_MS: &[u64] = &[3_000, 8_000, 15_000, 25_000];
const CAPACITY_ROUTE_TTL: Duration = Duration::from_secs(10 * 60);

static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

struct CachedRoute {
    runtime: String,
    endpoint: String,
    expires: Instant,
}

/// Cross-turn cache of the `(runtime_model, endpoint)` that answered after a
/// capacity walk, keyed by `(account_id, project_id, initial_runtime_model)`.
static PREFERRED_ROUTES: LazyLock<Mutex<HashMap<String, CachedRoute>>> = LazyLock::new(Default::default);
/// Per-turn retry nonce so retry POSTs get a fresh upstream `sessionId`.
static TURN_NONCES: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(Default::default);

// ---------------------------------------------------------------- tool & schema conversion

fn sanitize_tool_call_id(id: &str, fallback_name: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        let n = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}_{n}", if fallback_name.is_empty() { "tool" } else { fallback_name })
    } else {
        cleaned
    }
}

fn tool_call_id_needed(public_model: &str, runtime_model: &str) -> bool {
    public_model.starts_with("claude-")
        || public_model.starts_with("gpt-oss-")
        || runtime_model.starts_with("claude-")
        || runtime_model.starts_with("gpt-oss-")
}

fn is_valid_thought_signature(sig: &str) -> bool {
    !sig.is_empty()
        && sig.len().is_multiple_of(4)
        && sig.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn gemini_requires_thought_signature(runtime_model: &str) -> bool {
    let Some(rest) = runtime_model.strip_prefix("gemini-") else { return false };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.parse::<u32>() {
        Ok(major) => major >= 3,
        Err(_) => true,
    }
}

fn resolve_local_pointer<'a>(pointer: &str, root: &'a Value) -> Option<&'a Value> {
    if pointer == "#" {
        return Some(root);
    }
    let rest = pointer.strip_prefix("#/")?;
    let mut cur = root;
    for token in rest.split('/') {
        let key = token.replace("~1", "/").replace("~0", "~");
        match cur {
            Value::Array(arr) => {
                let idx: usize = key.parse().ok()?;
                cur = arr.get(idx)?;
            }
            Value::Object(map) => {
                cur = map.get(&key)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

fn is_schema_map_keyword(k: &str) -> bool {
    matches!(k, "properties" | "patternProperties" | "dependentSchemas" | "dependencies")
}

fn is_schema_value_or_array_keyword(k: &str) -> bool {
    matches!(
        k,
        "additionalItems"
            | "additionalProperties"
            | "contains"
            | "contentSchema"
            | "else"
            | "if"
            | "items"
            | "not"
            | "propertyNames"
            | "then"
            | "unevaluatedItems"
            | "unevaluatedProperties"
            | "allOf"
            | "anyOf"
            | "oneOf"
            | "prefixItems"
    )
}

fn deref_schema(schema: &Value, root: &Value, refs: &mut HashSet<String>, nodes: &mut usize, depth: usize) -> Result<Value, String> {
    if depth > MAX_SCHEMA_DEREF_DEPTH {
        return Err("schema depth limit exceeded".into());
    }
    *nodes += 1;
    if *nodes > MAX_SCHEMA_DEREF_NODES {
        return Err("schema node limit exceeded".into());
    }
    match schema {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(deref_schema(item, root, refs, nodes, depth + 1)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            if let Some(ref_str) = map.get("$ref").and_then(Value::as_str) {
                if !refs.insert(ref_str.to_string()) {
                    return Err(format!("circular $ref {ref_str}"));
                }
                let target = resolve_local_pointer(ref_str, root).ok_or_else(|| format!("unresolved $ref {ref_str}"))?;
                let resolved = deref_schema(target, root, refs, nodes, depth + 1)?;
                refs.remove(ref_str);
                let mut siblings = Map::new();
                for (k, v) in map {
                    if k != "$ref" {
                        siblings.insert(k.clone(), v.clone());
                    }
                }
                let sibling_val = deref_schema(&Value::Object(siblings), root, refs, nodes, depth + 1)?;
                if let (Value::Object(mut base), Value::Object(extra)) = (resolved.clone(), sibling_val) {
                    for (k, v) in extra {
                        base.insert(k, v);
                    }
                    return Ok(Value::Object(base));
                }
                return Ok(resolved);
            }
            let mut out = Map::new();
            for (k, v) in map {
                if k == "$defs" || k == "definitions" {
                    continue;
                }
                let next = if is_schema_map_keyword(k) {
                    if let Some(inner) = v.as_object() {
                        let mut m = Map::new();
                        for (pk, pv) in inner {
                            m.insert(pk.clone(), deref_schema(pv, root, refs, nodes, depth + 1)?);
                        }
                        Value::Object(m)
                    } else {
                        deref_schema(v, root, refs, nodes, depth + 1)?
                    }
                } else if is_schema_value_or_array_keyword(k) {
                    deref_schema(v, root, refs, nodes, depth + 1)?
                } else {
                    v.clone()
                };
                out.insert(k.clone(), next);
            }
            Ok(Value::Object(out))
        }
        _ => Ok(schema.clone()),
    }
}

fn strip_meta_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.iter().map(strip_meta_schema).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema" | "$id" | "$anchor" | "$dynamicAnchor" | "$vocabulary" | "$comment" | "$defs" | "definitions"
                ) {
                    continue;
                }
                let next = if is_schema_map_keyword(k) {
                    if let Some(inner) = v.as_object() {
                        Value::Object(inner.iter().map(|(pk, pv)| (pk.clone(), strip_meta_schema(pv))).collect())
                    } else {
                        strip_meta_schema(v)
                    }
                } else if is_schema_value_or_array_keyword(k) {
                    strip_meta_schema(v)
                } else {
                    v.clone()
                };
                out.insert(k.clone(), next);
            }
            Value::Object(out)
        }
        _ => schema.clone(),
    }
}

/// Allowlist normalization for Cloud Code Assist's Claude/GPT-OSS custom-tool
/// bridge (`parameters` field), which uses Protobuf `Schema` and rejects unknown
/// JSON Schema keywords with HTTP 400.
fn normalize_custom_tool_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_custom_tool_schema).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if !matches!(k.as_str(), "type" | "description" | "properties" | "required" | "items" | "enum") {
                    continue;
                }
                if k == "type" {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), Value::String(s.to_string()));
                    } else if let Some(arr) = v.as_array()
                        && let Some(scalar) = arr.iter().filter_map(Value::as_str).find(|s| *s != "null")
                    {
                        out.insert(k.clone(), Value::String(scalar.to_string()));
                    }
                    continue;
                }
                if k == "properties"
                    && let Some(props) = v.as_object()
                {
                    let cleaned: Map<String, Value> = props.iter().map(|(pk, pv)| (pk.clone(), normalize_custom_tool_schema(pv))).collect();
                    out.insert(k.clone(), Value::Object(cleaned));
                    continue;
                }
                if k == "enum"
                    && let Some(arr) = v.as_array()
                    && !arr.iter().all(Value::is_string)
                {
                    continue;
                }
                out.insert(k.clone(), normalize_custom_tool_schema(v));
            }
            Value::Object(out)
        }
        _ => schema.clone(),
    }
}

fn ensure_root_object(schema: Value) -> Value {
    match schema {
        Value::Object(mut m) => {
            if !m.contains_key("type") {
                m.insert("type".into(), json!("object"));
                m.entry("properties").or_insert_with(|| json!({}));
            }
            Value::Object(m)
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// Converts Oak tool definitions to Antigravity `functionDeclarations`.
pub(crate) fn wire_tools(tools: &[Value], use_legacy_parameters: bool) -> Vec<Value> {
    let mut decls = Vec::new();
    for t in tools {
        let Some(name) = t["name"].as_str().filter(|s| !s.is_empty()) else { continue };
        let desc = t["description"].as_str().unwrap_or("");
        let raw_schema = &t["input_schema"];
        let mut refs = HashSet::new();
        let mut nodes = 0;
        let Ok(deref) = deref_schema(raw_schema, raw_schema, &mut refs, &mut nodes, 0) else { continue };
        let schema = strip_meta_schema(&ensure_root_object(deref));
        let decl = if use_legacy_parameters {
            json!({"name": name, "description": desc, "parameters": normalize_custom_tool_schema(&schema)})
        } else {
            json!({"name": name, "description": desc, "parametersJsonSchema": schema})
        };
        decls.push(decl);
    }
    decls
}

/// Preamble helper for `api::preamble`.
pub fn preamble(model: &str, system: &str, tools: &[Value]) -> (String, Vec<Value>) {
    let pub_id = catalog::public_id(model);
    let legacy = pub_id.starts_with("claude-") || pub_id.starts_with("gpt-oss-");
    (system.to_string(), wire_tools(tools, legacy))
}

// ---------------------------------------------------------------- message conversion

fn append_turn(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut()
        && last["role"].as_str() == Some(role)
        && let Some(arr) = last["parts"].as_array_mut()
    {
        arr.append(&mut parts);
        return;
    }
    contents.push(json!({"role": role, "parts": parts}));
}

fn user_block_part(b: &Value) -> Option<Value> {
    match b["type"].as_str()? {
        "text" => {
            let t = b["text"].as_str().unwrap_or("");
            (!t.trim().is_empty()).then(|| json!({"text": t}))
        }
        "image" => {
            let data = b["data"].as_str().filter(|s| !s.is_empty())?;
            let mime = b["media_type"].as_str().unwrap_or("image/png");
            Some(json!({"inlineData": {"mimeType": mime, "data": data}}))
        }
        "document" => {
            let title = b["title"].as_str().unwrap_or("document");
            let mime = b["media_type"].as_str().unwrap_or("application/pdf");
            let data = b["data"].as_str().unwrap_or("");
            if mime == "text/plain" {
                Some(json!({"text": format!("{title}:\n{data}")}))
            } else if !data.is_empty() {
                Some(json!({"inlineData": {"mimeType": mime, "data": data}}))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn block_thought_sig<'a>(b: &'a Value, model_id: &str) -> Option<&'a str> {
    let by_ok = b["by"].as_str() == Some("antigravity");
    let model_ok = b["sig_model"].as_str().is_none_or(|m| catalog::public_id(m) == catalog::public_id(model_id));
    if !by_ok || !model_ok {
        return None;
    }
    b["sig"].as_str().filter(|s| is_valid_thought_signature(s))
}

fn convert_messages(messages: &[Value], public_model: &str, runtime_model: &str) -> (Vec<Value>, bool) {
    let mut contents: Vec<Value> = Vec::new();
    let mut degraded = false;
    let requires_sig = gemini_requires_thought_signature(runtime_model);
    let need_ids = tool_call_id_needed(public_model, runtime_model);
    // Tool call id -> serialized args JSON for unsigned tool calls degraded to text.
    let mut dropped_ids: HashMap<String, String> = HashMap::new();
    // Call id -> tool name (Oak's `tool_result` has `call_id` but not `name`).
    let mut call_names: HashMap<String, String> = HashMap::new();

    for m in messages {
        let role = m["role"].as_str().unwrap_or("");
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        if role == "system" {
            let parts: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(user_block_part)
                .collect();
            append_turn(&mut contents, "user", parts);
            continue;
        }
        if role == "assistant" {
            let mut parts: Vec<Value> = Vec::new();
            let tool_calls: Vec<&Value> = blocks.iter().filter(|b| b["type"] == "tool_call").collect();
            let first_has_sig = tool_calls.first().and_then(|tc| block_thought_sig(tc, public_model)).is_some();
            let all_sigs_valid = tool_calls.iter().all(|tc| tc["sig"].is_null() || block_thought_sig(tc, public_model).is_some());
            let group_signed = first_has_sig && all_sigs_valid;

            for b in &blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => {
                        let text = b["text"].as_str().unwrap_or("");
                        let sig = block_thought_sig(b, public_model);
                        if text.trim().is_empty() && sig.is_none() {
                            continue;
                        }
                        let mut p = json!({"text": text});
                        if let Some(s) = sig {
                            p["thoughtSignature"] = json!(s);
                        }
                        parts.push(p);
                    }
                    "reasoning" => {
                        let text = b["text"].as_str().unwrap_or("");
                        if text.trim().is_empty() || b["by"].as_str() != Some("antigravity") {
                            continue;
                        }
                        let sig = block_thought_sig(b, public_model);
                        if requires_sig && sig.is_none() && !group_signed {
                            continue;
                        }
                        let mut p = json!({"thought": true, "text": text});
                        if let Some(s) = sig {
                            p["thoughtSignature"] = json!(s);
                        }
                        parts.push(p);
                    }
                    "tool_call" => {
                        let raw_id = b["id"].as_str().unwrap_or("");
                        let name = b["name"].as_str().unwrap_or("tool");
                        let args = if b["input"].is_object() { b["input"].clone() } else { json!({}) };
                        if !raw_id.is_empty() {
                            call_names.insert(raw_id.to_string(), name.to_string());
                        }
                        if requires_sig && !group_signed {
                            dropped_ids.insert(raw_id.to_string(), args.to_string());
                        } else {
                            dropped_ids.remove(raw_id);
                            let mut fc = json!({"name": name, "args": args});
                            if need_ids {
                                fc["id"] = json!(sanitize_tool_call_id(raw_id, name));
                            }
                            let mut p = json!({"functionCall": fc});
                            if let Some(s) = block_thought_sig(b, public_model) {
                                p["thoughtSignature"] = json!(s);
                            }
                            parts.push(p);
                        }
                    }
                    _ => {}
                }
            }
            if parts.is_empty() && requires_sig && !group_signed && !tool_calls.is_empty() {
                parts.push(json!({"text": "[Earlier tool call omitted because its signature is unavailable.]"}));
            }
            append_turn(&mut contents, "model", parts);
            continue;
        }

        // User turn (may contain text, images, documents, and tool_result blocks).
        let mut user_parts: Vec<Value> = Vec::new();
        for b in &blocks {
            if b["type"] == "tool_result" {
                let raw_id = b["call_id"].as_str().unwrap_or("");
                let name = call_names.get(raw_id).map(String::as_str).unwrap_or("tool");
                let is_err = b["is_error"] == true;
                let mut text_chunks: Vec<String> = Vec::new();
                let mut img_parts: Vec<Value> = Vec::new();
                match &b["content"] {
                    Value::String(s) => text_chunks.push(s.clone()),
                    Value::Array(arr) => {
                        for item in arr {
                            match item["type"].as_str().unwrap_or("") {
                                "text" => {
                                    if let Some(t) = item["text"].as_str() {
                                        text_chunks.push(t.to_string());
                                    }
                                }
                                "image" => {
                                    if let Some(p) = user_block_part(item) {
                                        img_parts.push(p);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                let joined = text_chunks.join("\n");
                let response_text = if joined.is_empty() && is_err { "Tool failed".to_string() } else { joined };

                if requires_sig && let Some(dropped_args) = dropped_ids.remove(raw_id) {
                    let first_degraded = !degraded;
                    degraded = true;
                    let label = if dropped_args == "{}" { format!("`{name}`") } else { format!("`{name}` ({dropped_args})") };
                    user_parts.push(json!({"text": format!("[Observation from {label}:\n{response_text}]")}));
                    if first_degraded {
                        user_parts.push(json!({
                            "text": "[History note: Earlier \"[Observation from ...]\" blocks are historical tool results, not a format to imitate. To use a tool now, emit a real functionCall.]"
                        }));
                    }
                    user_parts.extend(img_parts);
                } else {
                    let resp_obj = if is_err { json!({"error": response_text}) } else { json!({"output": response_text}) };
                    let mut fr = json!({"name": name, "response": resp_obj});
                    if need_ids {
                        fr["id"] = json!(sanitize_tool_call_id(raw_id, name));
                    }
                    user_parts.push(json!({"functionResponse": fr}));
                    user_parts.extend(img_parts);
                }
            } else if let Some(p) = user_block_part(b) {
                user_parts.push(p);
            }
        }
        append_turn(&mut contents, "user", user_parts);
    }

    // Ensure every model turn with a functionCall follows a user turn.
    let mut i = 0;
    while i < contents.len() {
        let is_fn_model = contents[i]["role"] == "model"
            && contents[i]["parts"].as_array().is_some_and(|ps| ps.iter().any(|p| p.get("functionCall").is_some()));
        let prev_is_user = i > 0 && contents[i - 1]["role"] == "user";
        if is_fn_model && !prev_is_user {
            contents.insert(i, json!({"role": "user", "parts": [{"text": CONTINUATION_TEXT}]}));
            i += 2;
        } else {
            i += 1;
        }
    }

    // Claude & GPT-OSS require all functionResponse parts contiguous at the start
    // of the following user turn, matching every functionCall id.
    if need_ids {
        let mut idx = 0;
        while idx + 1 < contents.len() {
            let calls: Vec<(String, String)> = if contents[idx]["role"] == "model" {
                contents[idx]["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| {
                        let fc = p.get("functionCall")?;
                        Some((fc["name"].as_str()?.to_string(), fc["id"].as_str()?.to_string()))
                    })
                    .collect()
            } else {
                vec![]
            };
            if !calls.is_empty() {
                if contents[idx + 1]["role"] != "user" {
                    contents.insert(idx + 1, json!({"role": "user", "parts": []}));
                }
                let next_parts = contents[idx + 1]["parts"].as_array().cloned().unwrap_or_default();
                let mut responses: Vec<Value> = next_parts.iter().filter(|p| p.get("functionResponse").is_some()).cloned().collect();
                let others: Vec<Value> = next_parts.iter().filter(|p| p.get("functionResponse").is_none()).cloned().collect();
                let mut seen: HashSet<String> = responses
                    .iter()
                    .filter_map(|p| p["functionResponse"]["id"].as_str().map(str::to_string))
                    .collect();
                for (name, id) in calls {
                    if !seen.contains(&id) {
                        responses.push(json!({
                            "functionResponse": {
                                "name": name,
                                "id": id,
                                "response": {"error": "Tool execution was interrupted before a result was recorded."}
                            }
                        }));
                        seen.insert(id);
                    }
                }
                responses.extend(others);
                contents[idx + 1]["parts"] = Value::Array(responses);
            }
            idx += 1;
        }
    }

    // Ensure at least one non-empty user text part exists.
    let has_user_text = contents.iter().any(|t| {
        t["role"] == "user"
            && t["parts"]
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| p["text"].as_str().is_some_and(|s| !s.trim().is_empty())))
    });
    if !has_user_text && !contents.is_empty() {
        if let Some(u) = contents.iter_mut().find(|t| t["role"] == "user")
            && let Some(ps) = u["parts"].as_array_mut()
        {
            ps.push(json!({"text": CONTINUATION_TEXT}));
        } else {
            contents.insert(0, json!({"role": "user", "parts": [{"text": CONTINUATION_TEXT}]}));
        }
    }

    // Request cannot end on a model turn.
    if contents.last().is_some_and(|t| t["role"] == "model") {
        append_turn(&mut contents, "user", vec![json!({"text": CONTINUATION_TEXT})]);
    }

    (contents, degraded)
}

// ---------------------------------------------------------------- request envelope

fn should_inline_system(system_text: &str) -> bool {
    if let Some(v) = env_var("INLINE_SYSTEM") {
        match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "on" => return true,
            "0" | "false" | "off" => return false,
            _ => {}
        }
    }
    system_text.chars().count() > SYSTEM_INSTRUCTION_PART_LIMIT
}

fn to_int64_session_id(seed: &str) -> String {
    let hash = Sha256::digest(format!("antigravity:session:{seed}").as_bytes());
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&hash[..8]);
    i64::from_le_bytes(buf).to_string()
}

fn credits_enabled() -> bool {
    matches!(env_var("CREDITS").as_deref().map(str::to_ascii_lowercase).as_deref(), Some("1" | "true" | "on"))
}

/// Builds the full Antigravity `streamGenerateContent` JSON request body for `req`.
#[cfg(test)]
pub(crate) fn request_body(req: &Request) -> Value {
    let effort = if !req.thinking { None } else { req.effort.as_deref() };
    let runtime_model = env_var("RUNTIME_MODEL").unwrap_or_else(|| catalog::request_model_id(&req.model, effort));
    request_body_for(req, "antigravity-project", &runtime_model, 0, api::now_ms())
}

pub(crate) fn request_body_for(req: &Request, project_id: &str, runtime_model: &str, retry_nonce: u32, now_ms: i64) -> Value {
    let pub_id = catalog::public_id(&req.model);
    let msgs = req.messages_with_instruction();
    let (mut contents, _degraded) = convert_messages(&msgs, pub_id, runtime_model);

    let system_text = req.system.trim();
    let system_parts: Vec<Value> = if !system_text.is_empty() {
        vec![json!({"text": system_text})]
    } else {
        vec![json!({"text": "You are a coding assistant helping the user with their project."})]
    };

    let has_user_text = contents.iter().any(|t| {
        t["role"] == "user"
            && t["parts"]
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| p["text"].as_str().is_some_and(|s| !s.trim().is_empty())))
    });
    let injected_placeholder = !has_user_text;
    if injected_placeholder {
        contents.insert(0, json!({"role": "user", "parts": [{"text": PLACEHOLDER_SYSTEM_TURN_TEXT}]}));
    }

    let combined_system: String = system_parts
        .iter()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut inner = Map::new();
    if should_inline_system(&combined_system) {
        if let Some(first) = contents.first_mut()
            && first["role"] == "user"
            && let Some(parts) = first["parts"].as_array_mut()
        {
            if injected_placeholder && !parts.is_empty() {
                parts.remove(0);
            }
            parts.insert(0, json!({"text": combined_system}));
        } else {
            contents.insert(0, json!({"role": "user", "parts": [{"text": combined_system}]}));
        }
    } else {
        inner.insert("systemInstruction".into(), json!({"role": "user", "parts": system_parts}));
    }

    // Generation config
    let mut gen_cfg = Map::new();
    let effort_opt = if !req.thinking { Some("off") } else { req.effort.as_deref() };
    if let Some(tc) = catalog::thinking_config(runtime_model, effort_opt) {
        gen_cfg.insert("thinkingConfig".into(), tc);
    }
    let max_allowed = catalog::max_output_for(&req.model, Some(runtime_model));
    let max_out = if req.max_tokens > 0 { req.max_tokens.min(max_allowed) } else { max_allowed };
    gen_cfg.insert("maxOutputTokens".into(), json!(max_out));
    inner.insert("generationConfig".into(), Value::Object(gen_cfg));

    // Tools
    let is_claude = pub_id.starts_with("claude-") || runtime_model.starts_with("claude-");
    let is_gpt_oss = pub_id.starts_with("gpt-oss-") || runtime_model.starts_with("gpt-oss-");
    let decls = wire_tools(&req.tools, is_claude || is_gpt_oss);
    if !decls.is_empty() {
        inner.insert("tools".into(), json!([{"functionDeclarations": decls}]));
        if req.tools_off {
            inner.insert("toolConfig".into(), json!({"functionCallingConfig": {"mode": "NONE"}}));
        }
    }

    // Envelope metadata matching agy CLI / pi-antigravity wire alignment.
    let step = contents.len().max(1);
    let last_step_index = step.saturating_sub(1).to_string();
    let request_index = req.messages.iter().filter(|m| m["role"] == "assistant").count();
    let seed = match &req.conv {
        Some(c) if !c.is_empty() => format!("session:{c}"),
        _ => format!("anon:{}:{}", req.model, req.messages.first().map(Value::to_string).unwrap_or_default()),
    };
    let effective_seed = if retry_nonce == 0 { seed.clone() } else { format!("{seed}:r{retry_nonce}") };
    let conversation_id = stable_uuid(&format!("antigravity:conv:{effective_seed}"));
    let trajectory_id = stable_uuid(&format!("antigravity:traj:{effective_seed}"));
    let session_id = to_int64_session_id(&effective_seed);

    let is_non_gemini = is_claude || is_gpt_oss || (!pub_id.starts_with("gemini-") && !runtime_model.starts_with("gemini-"));
    let mut labels = Map::new();
    labels.insert("last_step_index".into(), json!(last_step_index));
    labels.insert("request_id".into(), json!(format!("{trajectory_id}-{request_index}")));
    labels.insert("trajectory_id".into(), json!(trajectory_id));
    labels.insert("used_claude".into(), json!(if is_claude { "true" } else { "false" }));
    labels.insert("used_claude_conservative".into(), json!(if is_claude { "true" } else { "false" }));
    labels.insert("used_non_gemini_model".into(), json!(if is_non_gemini { "true" } else { "false" }));
    if let Some(me) = catalog::model_enum(runtime_model) {
        labels.insert("model_enum".into(), json!(me));
    }
    if step > 1 {
        labels.insert("last_execution_id".into(), json!(stable_uuid(&format!("antigravity:exec:{trajectory_id}:{}", step - 1))));
    }

    inner.insert("contents".into(), Value::Array(contents));
    inner.insert("sessionId".into(), json!(session_id));
    inner.insert("labels".into(), Value::Object(labels));

    let mut body = json!({
        "project": project_id,
        "model": runtime_model,
        "request": Value::Object(inner),
        "requestType": "agent",
        "userAgent": "antigravity",
        "requestId": format!("agent/{conversation_id}/{now_ms}/{trajectory_id}/{step}"),
    });
    if credits_enabled() {
        body["enabledCreditTypes"] = json!(["GOOGLE_ONE_AI"]);
    }
    body
}

// ---------------------------------------------------------------- SSE translation

#[derive(Debug, PartialEq)]
pub(crate) enum Out {
    Event(Event),
    Done,
    Err(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveKind {
    Thinking,
    Text,
}

/// Translates Cloud Code Assist SSE JSON chunks into Oak `Event`s.
pub(crate) struct Translate {
    model: String,
    started: bool,
    next: usize,
    active: Option<(usize, ActiveKind)>,
    has_tools: bool,
    has_content: bool,
    saw_finish: bool,
    finish_reason: Option<String>,
    usage: oak::Usage,
}

impl Translate {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            started: false,
            next: 0,
            active: None,
            has_tools: false,
            has_content: false,
            saw_finish: false,
            finish_reason: None,
            usage: oak::Usage::default(),
        }
    }

    fn ensure_started(&mut self, data: &Value, out: &mut Vec<Out>) {
        if !self.started {
            self.started = true;
            let id = data["responseId"].as_str().unwrap_or("").to_string();
            let model = data["modelVersion"].as_str().unwrap_or(&self.model).to_string();
            out.push(Out::Event(Event::Start { id, model }));
        }
    }

    fn close_active(&mut self, out: &mut Vec<Out>) {
        if let Some((idx, _)) = self.active.take() {
            out.push(Out::Event(Event::BlockEnd { index: idx }));
        }
    }

    pub(crate) fn chunk(&mut self, chunk: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        if let Some(err) = chunk.get("error").filter(|v| !v.is_null()) {
            let status = err["code"].as_u64().map(|n| n as u16);
            let msg = friendly_error(status, &json!({"error": err}).to_string());
            out.push(Out::Err(msg));
            return out;
        }
        let data = chunk.get("response").filter(|v| v.is_object()).unwrap_or(chunk);
        let candidate = data["candidates"].as_array().and_then(|a| a.first());

        if let Some(parts) = candidate.and_then(|c| c["content"]["parts"].as_array()) {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    self.has_content = true;
                    let is_thought = part["thought"].as_bool() == Some(true);
                    let want = if is_thought { ActiveKind::Thinking } else { ActiveKind::Text };
                    if self.active.is_none_or(|(_, k)| k != want) {
                        self.close_active(&mut out);
                        self.ensure_started(data, &mut out);
                        let index = self.next;
                        self.next += 1;
                        let block = if is_thought {
                            json!({"type": "reasoning", "text": "", "by": "antigravity", "sig_model": &self.model})
                        } else {
                            json!({"type": "text", "text": "", "by": "antigravity", "sig_model": &self.model})
                        };
                        out.push(Out::Event(Event::Block { index, block }));
                        self.active = Some((index, want));
                    }
                    let index = self.active.unwrap().0;
                    if !text.is_empty() {
                        out.push(Out::Event(if is_thought {
                            Event::Reasoning { index, text: text.to_string() }
                        } else {
                            Event::Text { index, text: text.to_string() }
                        }));
                    }
                    if let Some(sig) = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()) {
                        out.push(Out::Event(Event::Sig { index, sig: sig.to_string() }));
                    }
                }

                if let Some(fc) = part.get("functionCall").filter(|v| v.is_object()) {
                    self.has_content = true;
                    self.has_tools = true;
                    self.close_active(&mut out);
                    self.ensure_started(data, &mut out);
                    let index = self.next;
                    self.next += 1;
                    let name = fc["name"].as_str().unwrap_or("");
                    let raw_id = fc["id"].as_str().unwrap_or("");
                    let id = sanitize_tool_call_id(raw_id, name);
                    let args = fc.get("args").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
                    let mut block = json!({"type": "tool_call", "id": id, "name": name, "input": {}, "by": "antigravity", "sig_model": &self.model});
                    if let Some(sig) = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()) {
                        block["sig"] = json!(sig);
                    }
                    out.push(Out::Event(Event::Block { index, block }));
                    out.push(Out::Event(Event::Input { index, json: args.to_string() }));
                    out.push(Out::Event(Event::BlockEnd { index }));
                }
            }
        }

        if let Some(u) = data.get("usageMetadata").filter(|v| v.is_object()) {
            let prompt = u["promptTokenCount"].as_u64().unwrap_or(0);
            let cache_read = u["cachedContentTokenCount"].as_u64().unwrap_or(0).min(prompt);
            let thoughts = u["thoughtsTokenCount"].as_u64().unwrap_or(0);
            let candidates = u["candidatesTokenCount"].as_u64().unwrap_or(0);
            self.usage.merge(&oak::Usage {
                input: prompt - cache_read,
                cache_read,
                output: candidates + thoughts,
                reasoning: (thoughts > 0).then_some(thoughts),
                ..Default::default()
            });
        }

        if let Some(reason) = candidate.and_then(|c| c["finishReason"].as_str()).filter(|s| !s.is_empty()) {
            self.saw_finish = true;
            self.finish_reason = Some(reason.to_string());
        }

        out
    }

    pub(crate) fn finish(mut self) -> Vec<Out> {
        let mut out = Vec::new();
        self.close_active(&mut out);
        if !self.saw_finish {
            out.push(Out::Err("Antigravity stream ended before the reply was complete".into()));
            return out;
        }
        out.push(Out::Event(Event::Usage(self.usage)));
        let (reason, detail) = if self.has_tools {
            (oak::stop::TOOL_CALLS.to_string(), None)
        } else {
            match self.finish_reason.as_deref() {
                Some("STOP") | None => (oak::stop::END.to_string(), None),
                Some("MAX_TOKENS") => (oak::stop::MAX_TOKENS.to_string(), None),
                Some("SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII") => (oak::stop::REFUSAL.to_string(), self.finish_reason.clone()),
                Some(other) => (oak::stop::END.to_string(), Some(other.to_string())),
            }
        };
        out.push(Out::Event(Event::Stop { reason, detail }));
        out.push(Out::Done);
        out
    }
}

// ---------------------------------------------------------------- error classification & reset parsing

/// True when a 429 body indicates an account quota wall (fail over to another login).
pub(crate) fn is_account_quota_429(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("individual quota reached")
        || lower.contains("resets in ")
        || lower.contains("reset in ")
        || lower.contains("ai credits balance is too low")
        || lower.contains("insufficient_g1_credits")
    {
        return true;
    }
    let is_rl = lower.contains("rate limit") || lower.contains("rate_limit") || lower.contains("ratelimit");
    !is_rl && (lower.contains("quota exceeded") || lower.contains("exceeded your") || lower.contains("daily limit"))
}

pub(crate) fn is_retryable_rate_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("rate limit") || lower.contains("rate_limit") || lower.contains("ratelimit") || lower.contains("too many requests")
}

/// Generic 429 RESOURCE_EXHAUSTED with no reset hint (transient capacity or dead tier).
pub(crate) fn is_generic_resource_exhausted(text: &str) -> bool {
    if is_account_quota_429(text) || is_retryable_rate_limit(text) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    lower.contains("resource has been exhausted") || lower.contains("resource_exhausted")
}

pub(crate) fn is_capacity_failure(status: u16, text: &str) -> bool {
    (status == 429 && is_generic_resource_exhausted(text)) || (status == 503 && text.to_ascii_lowercase().contains("no capacity available"))
}

/// Parses a reset duration like `"Resets in 2h 15m 30s"` or `"Resets in 45m"` into epoch ms.
pub(crate) fn parse_reset_time_ms(text: &str, now_ms: i64) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("resets in ").map(|i| i + 10).or_else(|| lower.find("reset in ").map(|i| i + 9))?;
    let tail = &lower[idx..];
    let mut total_secs: i64 = 0;
    let mut num: i64 = 0;
    let mut have_num = false;
    let mut matched_any = false;
    for ch in tail.chars() {
        if ch.is_ascii_digit() {
            num = num.saturating_mul(10).saturating_add((ch as u8 - b'0') as i64);
            have_num = true;
        } else if have_num {
            let mult = match ch {
                'd' => Some(86_400),
                'h' => Some(3_600),
                'm' => Some(60),
                's' => Some(1),
                _ => None,
            };
            if let Some(m) = mult {
                total_secs = total_secs.saturating_add(num.saturating_mul(m));
                matched_any = true;
                num = 0;
                have_num = false;
            } else if ch.is_whitespace() {
                continue;
            } else {
                break;
            }
        } else if ch.is_whitespace() {
            continue;
        } else {
            break;
        }
    }
    (matched_any && total_secs > 0).then(|| now_ms + total_secs * 1000)
}

pub(crate) fn friendly_error(status: Option<u16>, text: &str) -> String {
    let raw = match status {
        Some(s) => error_text(s, text),
        None => text.to_string(),
    };
    let lower = raw.to_ascii_lowercase();
    match status {
        Some(401) => "Antigravity authentication failed: sign in again on Home.".into(),
        Some(403) => {
            if lower.contains("#3501") || lower.contains("do not have a valid license") {
                format!("Antigravity denied access (license not accepted on consumer endpoint): {raw}")
            } else {
                format!("Antigravity denied access for this account or model: {raw}")
            }
        }
        Some(404) => format!("Antigravity model not available right now: {raw}"),
        Some(429) => {
            if lower.contains("ai credits balance is too low") || lower.contains("insufficient_g1_credits") {
                "Antigravity AI credits balance is exhausted; wait for regular quota reset or switch models.".into()
            } else if is_account_quota_429(text) {
                format!("Antigravity quota reached: {raw}")
            } else if is_generic_resource_exhausted(text) {
                format!("Antigravity model is temporarily out of capacity: {raw}")
            } else {
                format!("Antigravity rate limited: {raw}")
            }
        }
        _ => format!("Antigravity {raw}"),
    }
}

// ---------------------------------------------------------------- streaming & failover

fn next_retry_nonce(turn_key: &str) -> u32 {
    let mut map = TURN_NONCES.lock().unwrap();
    if map.len() > 64 {
        map.clear();
    }
    let entry = map.entry(turn_key.to_string()).or_insert(0);
    let val = *entry;
    *entry = val.saturating_add(1);
    val
}

fn remembered_route(key: &str) -> Option<(String, String)> {
    let mut map = PREFERRED_ROUTES.lock().unwrap();
    if let Some(entry) = map.get(key) {
        if entry.expires > Instant::now() {
            return Some((entry.runtime.clone(), entry.endpoint.clone()));
        }
        map.remove(key);
    }
    None
}

fn remember_route(key: &str, runtime_model: &str, endpoint: &str) {
    let mut map = PREFERRED_ROUTES.lock().unwrap();
    if map.len() > 64 {
        map.clear();
    }
    map.insert(
        key.to_string(),
        CachedRoute { runtime: runtime_model.to_string(), endpoint: endpoint.to_string(), expires: Instant::now() + CAPACITY_ROUTE_TTL },
    );
}

fn forget_route(key: &str) {
    PREFERRED_ROUTES.lock().unwrap().remove(key);
}

/// Streams one Oak request through Antigravity, sending Oak `Sse` events to `tx`.
pub fn stream(req: Request, tx: Sender<Sse>) {
    let conv = req.seat_conv();
    let effort = if !req.thinking { None } else { req.effort.as_deref() };
    let base_runtime = env_var("RUNTIME_MODEL").unwrap_or_else(|| catalog::request_model_id(&req.model, effort));
    let is_known = catalog::is_known_model(&req.model);
    let allow_cache = env_var("RUNTIME_MODEL").is_none() && env_var("BASE_URL").is_none() && std::env::var_os("OAK_ANTIGRAVITY_URL").is_none();
    let endpoints = endpoint_candidates();
    let default_ep = endpoints.first().cloned().unwrap_or_else(|| super::DEFAULT_ENDPOINT.to_string());
    let turn_key = format!("{}:{}", req.conv.as_deref().unwrap_or("anon"), req.messages.iter().filter(|m| m["role"] == "assistant").count());

    let mut rotations = 0u32;
    let mut last_acct: Option<String> = crate::rotation::seat(api::Provider::Antigravity, &conv);
    let mut capacity_attempt: usize = 0;
    let mut empty_attempt: usize = 0;
    let mut renewed = false;

    loop {
        let acct_id = match auth::login_for(&conv, &req.model) {
            Ok(id) => id,
            Err(Some(s)) => return drop(tx.send(s)),
            Err(None) => return,
        };
        if last_acct.as_ref().is_some_and(|l| *l != acct_id) && tx.send(Sse::Rotated(auth::label_of(&acct_id))).is_err() {
            return;
        }
        last_acct = Some(acct_id.clone());

        let (token, project_id) = match auth::access(&acct_id, false) {
            Ok(pair) => pair,
            Err(e) => return drop(tx.send(Sse::Err(e))),
        };

        let mut initial_runtime = base_runtime.clone();
        if !is_known
            && env_var("RUNTIME_MODEL").is_none()
            && let Some(dyn_id) = catalog::fetch_available_runtime_model(&token, &project_id, &base_runtime, effort)
        {
            initial_runtime = dyn_id;
        }

        let route_key = format!("{acct_id}\0{project_id}\0{initial_runtime}");
        let cached = (allow_cache && capacity_attempt == 0 && empty_attempt == 0)
            .then(|| remembered_route(&route_key))
            .flatten();

        let mut runtime_candidates = vec![initial_runtime.clone()];
        if let Some(fb) = catalog::fallback_runtime_model(&initial_runtime, effort)
            && !runtime_candidates.contains(&fb)
        {
            runtime_candidates.push(fb);
        }

        let send_once = |target_runtime: &str, endpoint: &str, tok: &str| -> Result<reqwest::blocking::Response, String> {
            let nonce = next_retry_nonce(&turn_key);
            let body = request_body_for(&req, &project_id, target_runtime, nonce, api::now_ms());
            let url = format!("{endpoint}/v1internal:streamGenerateContent?alt=sse");
            api::send_fresh(|| with_headers(api::http().post(&url).header("Accept", "text/event-stream").json(&body), tok))
        };

        let mut open_resp: Option<(reqwest::blocking::Response, String, String, bool)> = None;
        let mut last_status: Option<u16> = None;
        let mut last_body = String::new();
        let mut last_net_err: Option<String> = None;

        // 1. Try cached fallback route first if we have one.
        if let Some((ref rt, ref ep)) = cached {
            match send_once(rt, ep, &token) {
                Ok(r) if r.status().is_success() => {
                    open_resp = Some((r, rt.clone(), ep.clone(), true));
                }
                Ok(r) => {
                    forget_route(&route_key);
                    last_status = Some(r.status().as_u16());
                    last_body = r.text().unwrap_or_default();
                }
                Err(e) => {
                    forget_route(&route_key);
                    last_net_err = Some(e);
                }
            }
        }

        // 2. Walk runtime candidates × endpoints.
        if open_resp.is_none() {
            let mut cand_idx = 0;
            'cands: while cand_idx < runtime_candidates.len() {
                let target_rt = runtime_candidates[cand_idx].clone();
                for ep in &endpoints {
                    match send_once(&target_rt, ep, &token) {
                        Ok(r) if r.status().is_success() => {
                            open_resp = Some((r, target_rt.clone(), ep.clone(), false));
                            break 'cands;
                        }
                        Ok(r) => {
                            let status = r.status().as_u16();
                            let text = r.text().unwrap_or_default();
                            last_status = Some(status);
                            last_body = text;
                            if status == 401 {
                                break 'cands;
                            }
                            if status == 429 && is_account_quota_429(&last_body) {
                                break 'cands;
                            }
                            if status == 429 && is_generic_resource_exhausted(&last_body) {
                                continue;
                            }
                            if status == 429 && !is_retryable_rate_limit(&last_body) {
                                break;
                            }
                            if !matches!(status, 403 | 404 | 429 | 500 | 502 | 503 | 504) {
                                break 'cands;
                            }
                        }
                        Err(e) => {
                            last_net_err = Some(e);
                        }
                    }
                }
                if last_status == Some(404) {
                    if cand_idx + 1 < runtime_candidates.len() {
                        cand_idx += 1;
                        continue;
                    }
                    if is_known
                        && let Some(dyn_id) = catalog::fetch_available_runtime_model(&token, &project_id, &base_runtime, effort)
                        && !runtime_candidates.contains(&dyn_id)
                    {
                        runtime_candidates.push(dyn_id);
                        cand_idx += 1;
                        continue;
                    }
                }
                if last_status == Some(429) && is_generic_resource_exhausted(&last_body) {
                    if let Some(sib) = catalog::advertised_sibling_runtimes(&target_rt)
                        .into_iter()
                        .find(|id| !runtime_candidates.contains(id))
                        && runtime_candidates.len() < 4
                    {
                        runtime_candidates.insert(cand_idx + 1, sib);
                        cand_idx += 1;
                        continue;
                    }
                    if cand_idx + 1 < runtime_candidates.len() {
                        cand_idx += 1;
                        continue;
                    }
                }
                break;
            }
        }

        let Some((resp, used_rt, used_ep, used_cached)) = open_resp else {
            // Handle 401 once by forcing OAuth token refresh.
            if last_status == Some(401) && !renewed {
                renewed = true;
                if let Err(e) = auth::access(&acct_id, true) {
                    return drop(tx.send(Sse::Err(e)));
                }
                continue;
            }
            // Handle 429 account quota wall: rotate to next account or wait until reset.
            if last_status == Some(429) && is_account_quota_429(&last_body) {
                let now = api::now_ms();
                let reset = parse_reset_time_ms(&last_body, now).unwrap_or(now + 15 * 60_000);
                if auth::limited(&acct_id, &req.model, reset) && rotations < 10 {
                    rotations += 1;
                    capacity_attempt = 0;
                    empty_attempt = 0;
                    renewed = false;
                    continue;
                }
                return drop(tx.send(Sse::Limit(reset)));
            }
            // Handle transient capacity failures (generic 429 / 503 No capacity available).
            if let Some(st) = last_status
                && is_capacity_failure(st, &last_body)
                && capacity_attempt < CAPACITY_BACKOFF_MS.len()
            {
                let wait_secs = (CAPACITY_BACKOFF_MS[capacity_attempt] / 1000).max(1);
                capacity_attempt += 1;
                if !provider::wait_between(&tx, capacity_attempt as u32, wait_secs, Some(friendly_error(Some(st), &last_body))) {
                    return;
                }
                continue;
            }
            // Retryable network or server/rate-limit error.
            if (last_status.is_none() || provider::retryable(last_status))
                && last_status.is_none_or(|st| st != 429 || is_retryable_rate_limit(&last_body))
                && capacity_attempt < api::RETRIES as usize
            {
                capacity_attempt += 1;
                let reason = last_status
                    .map(|st| friendly_error(Some(st), &last_body))
                    .or_else(|| last_net_err.clone());
                if !provider::wait_between(&tx, capacity_attempt as u32, api::retry_wait(capacity_attempt as u32), reason) {
                    return;
                }
                continue;
            }
            let err = match last_status {
                Some(st) => friendly_error(Some(st), &last_body),
                None => last_net_err.unwrap_or_else(|| "Antigravity request failed".into()),
            };
            return drop(tx.send(Sse::Err(err)));
        };

        let status = resp.status().as_u16();
        crate::rotation::used(api::Provider::Antigravity, &conv, &acct_id, oak::Ttl::FiveMinutes.ms(), api::now_ms());
        if !provider::say_headers(&tx, status) {
            return;
        }

        match forward(resp, &req.model, &tx) {
            StreamOutcome::Done => {
                if allow_cache && (used_rt != initial_runtime || used_ep != default_ep) {
                    remember_route(&route_key, &used_rt, &used_ep);
                }
                return;
            }
            StreamOutcome::Empty => {
                if used_cached {
                    forget_route(&route_key);
                }
                if empty_attempt < 2 {
                    empty_attempt += 1;
                    std::thread::sleep(Duration::from_millis(500 * (1u64 << (empty_attempt - 1))));
                    continue;
                }
                let _ = tx.send(Sse::Err("Antigravity returned an empty response".into()));
                return;
            }
            StreamOutcome::Err(e) => {
                if used_cached {
                    forget_route(&route_key);
                }
                let _ = tx.send(Sse::Err(e));
                return;
            }
            StreamOutcome::ReceiverGone => return,
        }
    }
}

enum StreamOutcome {
    Done,
    Empty,
    Err(String),
    ReceiverGone,
}

fn forward(resp: reqwest::blocking::Response, model: &str, tx: &Sender<Sse>) -> StreamOutcome {
    let mut t = Translate::new(model);
    let mut stray = String::new();
    for line in BufReader::new(resp).lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => return StreamOutcome::Err(format!("Stream broke: {e}")),
        };
        let trimmed = line.trim_end_matches('\r');
        if let Some(data) = trimmed.strip_prefix("data:") {
            let json_str = data.trim();
            if json_str.is_empty() || json_str == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(json_str) else { continue };
            for o in t.chunk(&v) {
                match o {
                    Out::Event(ev) => {
                        if tx.send(Sse::Event(ev)).is_err() {
                            return StreamOutcome::ReceiverGone;
                        }
                    }
                    Out::Done => {
                        let _ = tx.send(Sse::Done);
                        return StreamOutcome::Done;
                    }
                    Out::Err(e) => return StreamOutcome::Err(e),
                }
            }
        } else {
            let s = trimmed.trim();
            if !s.is_empty() && !s.starts_with(':') {
                if !stray.is_empty() {
                    stray.push('\n');
                }
                stray.push_str(s);
                if let Some(start) = stray.find('{')
                    && let Ok(v) = serde_json::from_str::<Value>(&stray[start..])
                    && v.get("error").is_some()
                {
                    let code = v["error"]["code"].as_u64().map(|n| n as u16);
                    return StreamOutcome::Err(friendly_error(code, &stray[start..]));
                }
            }
        }
    }
    if !t.has_content && !t.saw_finish {
        return StreamOutcome::Empty;
    }
    for o in t.finish() {
        match o {
            Out::Event(ev) => {
                if tx.send(Sse::Event(ev)).is_err() {
                    return StreamOutcome::ReceiverGone;
                }
            }
            Out::Done => {
                let _ = tx.send(Sse::Done);
                return StreamOutcome::Done;
            }
            Out::Err(e) => return StreamOutcome::Err(e),
        }
    }
    StreamOutcome::Done
}
