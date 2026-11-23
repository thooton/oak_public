use super::*;
use crate::oak::{self, Event, Request, Usage};
use crate::providers::Kind;
use serde_json::json;

#[test]
fn oauth_callback_parsing_and_metadata() {
    assert_eq!(
        auth::parse_pasted("http://localhost:51121/oauth-callback?state=st1&code=abc123"),
        Some(("abc123".into(), Some("st1".into())))
    );
    assert_eq!(
        auth::parse_pasted("state=s2&code=xyz"),
        Some(("xyz".into(), Some("s2".into())))
    );
    assert_eq!(auth::parse_pasted("http://localhost:51121/oauth-callback?error=access_denied"), None);
    assert_eq!(auth::parse_pasted("   "), None);

    assert_eq!(auth::extract_project_id(&json!({"cloudaicompanionProject": {"id": "proj-1"}})).as_deref(), Some("proj-1"));
    assert_eq!(auth::extract_project_id(&json!({"antigravityProjectId": "proj-2"})).as_deref(), Some("proj-2"));
    assert_eq!(
        auth::extract_plan(&json!({
            "currentTier": {"id": "free-tier", "name": "Free"},
            "paidTier": {"id": "g1-pro-tier", "name": "Google AI Pro"}
        }))
        .as_deref(),
        Some("Google AI Pro")
    );
    assert_eq!(auth::extract_plan(&json!({"currentTier": {"id": "free-tier", "name": "Free"}})).as_deref(), Some("Free"));
}

#[test]
fn provider_routing_never_collides_with_claude() {
    assert!(serves("antigravity/claude-opus-4-6"));
    assert!(serves("antigravity/gemini-3.8-flash"));
    assert!(serves("antigravity/gemini-4-neon"));
    assert!(!serves("claude-opus-4-6"));
    assert_eq!(Kind::of("antigravity/claude-opus-4-6"), Kind::Antigravity);
    assert_eq!(Kind::of("claude-opus-4-6"), Kind::Claude);
    assert_eq!(Kind::of("antigravity/gemini-3.8-flash"), Kind::Antigravity);
    assert_eq!(catalog::request_model_id("antigravity/gemini-3.8-flash", Some("high")), "gemini-3.8-flash-high");
    assert_eq!(catalog::request_model_id("antigravity/gemini-3.1-pro", Some("high")), "gemini-pro-agent");
    assert_eq!(catalog::request_model_id("antigravity/claude-opus-4-6", Some("high")), "claude-opus-4-6-thinking");
    assert_eq!(catalog::request_model_id("antigravity/gpt-oss-120b", Some("medium")), "gpt-oss-120b-medium");
    assert_eq!(model_name("antigravity/gemini-3.8-flash"), "Gemini 3.8 Flash");
    assert_eq!(context_window("antigravity/gemini-3.8-flash"), Some(1_048_576));
    assert_eq!(max_output("antigravity/gemini-4-neon"), 262_144);
    assert_eq!(efforts("antigravity/gemini-3.1-pro"), vec!["low", "high"]);
}

#[test]
fn dynamic_catalog_groups_variants_and_merges_agent_singletons() {
    let raw = json!({
        "gemini-4-neon-low": {"displayName": "Gemini 4 Neon (Low)", "maxTokens": 1048576, "maxOutputTokens": 262144, "supportsThinking": true, "supportsImages": true, "model": "MODEL_PLACEHOLDER_M534"},
        "gemini-4-neon-high": {"displayName": "Gemini 4 Neon (High)", "maxTokens": 1048576, "maxOutputTokens": 262144, "supportsThinking": true, "supportsImages": true, "model": "MODEL_PLACEHOLDER_M532"},
        "gemini-4-neon-xhigh": {"displayName": "Gemini 4 Neon (Extra High)", "maxTokens": 1048576, "maxOutputTokens": 262144, "supportsThinking": true, "supportsImages": true, "model": "MODEL_PLACEHOLDER_M535"},
        "gemini-3.1-pro-low": {"displayName": "Gemini 3.1 Pro (Low)", "maxTokens": 1048576, "maxOutputTokens": 65535, "supportsThinking": true, "supportsImages": true},
        "gemini-pro-agent": {"displayName": "Gemini 3.1 Pro (High)", "maxTokens": 1048576, "maxOutputTokens": 65535, "supportsThinking": true, "supportsImages": true},
        "chat_hidden": {"displayName": "Hidden Chat"},
        "gemini-3.1-flash-image": {"displayName": "Gemini 3.1 Flash Image"},
        "internal-model": {"displayName": "Internal", "isInternal": true},
        "disabled-model": {"displayName": "Disabled", "disabled": true}
    });
    let sorts = json!([{"groups": [{"modelIds": ["gemini-3.1-pro-low", "gemini-pro-agent"]}]}]);
    let cat = catalog::build_catalog(raw.as_object().unwrap(), Some(&sorts)).unwrap();
    let ids: Vec<&str> = cat.models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"antigravity/gemini-4-neon"), "neon should be extra-allowed: {ids:?}");
    assert!(ids.contains(&"antigravity/gemini-3.1-pro"), "pro should be present: {ids:?}");
    assert!(!ids.contains(&"antigravity/gemini-pro-agent"), "agent singleton should merge into gemini-3.1-pro");
    assert_eq!(ids.len(), 2, "internal/chat/image/disabled should be excluded: {ids:?}");

    let neon = cat.models.iter().find(|m| m.id == "antigravity/gemini-4-neon").unwrap();
    assert_eq!(neon.name, "Gemini 4 Neon");
    assert_eq!(neon.efforts, vec!["low", "high", "xhigh"]);
    assert_eq!(cat.model_enums.get("gemini-4-neon-high").map(String::as_str), Some("MODEL_PLACEHOLDER_M532"));

    let pro_route = cat.routing.get("gemini-3.1-pro").unwrap();
    assert_eq!(pro_route.low.as_deref(), Some("gemini-3.1-pro-low"));
    assert_eq!(pro_route.high.as_deref(), Some("gemini-pro-agent"));
}

#[test]
fn thinking_config_matches_model_families() {
    assert_eq!(catalog::thinking_config("gemini-4-argon-high", Some("off")), None);
    assert_eq!(catalog::thinking_config("gemini-4-neon-high", Some("high")), Some(json!({"includeThoughts": true, "thinkingBudget": -1})));
    assert_eq!(catalog::thinking_config("gemini-3.8-flash-medium", Some("medium")), Some(json!({"includeThoughts": true, "thinkingBudget": 4000})));
    assert_eq!(catalog::thinking_config("gemini-3.1-pro-low", Some("high")), Some(json!({"includeThoughts": true, "thinkingBudget": 10001})));
    assert_eq!(catalog::thinking_config("claude-opus-4-6-thinking", Some("high")), Some(json!({"includeThoughts": true, "thinkingBudget": 1024})));
    assert_eq!(catalog::thinking_config("gpt-oss-120b-medium", Some("medium")), Some(json!({"includeThoughts": true, "thinkingBudget": 8192})));
}

#[test]
fn request_conversion_system_cap_schemas_and_signatures() {
    let valid_sig = "QUFBQUFBQUE="; // valid base64, len 12 (% 4 == 0)
    let long_sys = "You are Oak. ".repeat(20);
    let tool = json!({
        "name": "READ",
        "description": "Read a file",
        "input_schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"Path": {"type": ["string", "null"], "description": "File path"}},
            "type": "object",
            "properties": {"path": {"$ref": "#/$defs/Path"}},
            "required": ["path"],
            "additionalProperties": false
        }
    });

    // 1. Gemini 3.8 Flash: long system prompt inlined, parametersJsonSchema used, signed tool_call kept.
    let req = Request {
        provider: Kind::Antigravity,
        model: "antigravity/gemini-3.8-flash".into(),
        max_tokens: 100_000,
        system: long_sys.clone(),
        thinking: true,
        effort: Some("high".into()),
        conv: Some("conv-1".into()),
        tools: vec![tool.clone()],
        messages: vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
            json!({"role": "assistant", "content": [
                {"type": "reasoning", "text": "thinking...", "by": "antigravity", "sig_model": "antigravity/gemini-3.8-flash", "sig": valid_sig},
                {"type": "tool_call", "id": "c1", "name": "READ", "input": {"path": "a.rs"}, "by": "antigravity", "sig_model": "antigravity/gemini-3.8-flash", "sig": valid_sig},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "call_id": "c1", "content": [{"type": "text", "text": "fn main() {}"}], "is_error": false}
            ]}),
        ],
        ..Default::default()
    };
    let body = wire::request_body_for(&req, "proj-1", "gemini-3.8-flash-high", 0, 1_700_000_000_000);
    assert_eq!(body["project"], "proj-1");
    assert_eq!(body["model"], "gemini-3.8-flash-high");
    assert_eq!(body["requestType"], "agent");
    assert_eq!(body["userAgent"], "antigravity");
    // System prompt > 126 chars must be inlined into contents[0], not systemInstruction.
    assert!(body["request"].get("systemInstruction").is_none());
    let contents = body["request"]["contents"].as_array().unwrap();
    assert_eq!(contents[0]["parts"][0]["text"].as_str().unwrap(), long_sys.trim());
    assert_eq!(contents[0]["parts"][1]["text"], "hello");
    // Signed thought & tool call preserved.
    assert_eq!(contents[1]["parts"][0]["thought"], true);
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], valid_sig);
    assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "READ");
    assert_eq!(contents[1]["parts"][1]["thoughtSignature"], valid_sig);
    assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "READ");
    assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["output"], "fn main() {}");
    // Gemini tool uses parametersJsonSchema with $ref resolved and $schema/$defs stripped.
    let decl = &body["request"]["tools"][0]["functionDeclarations"][0];
    assert!(decl.get("parametersJsonSchema").is_some());
    assert!(decl["parametersJsonSchema"].get("$schema").is_none());
    assert!(decl["parametersJsonSchema"].get("$defs").is_none());
    assert_eq!(decl["parametersJsonSchema"]["properties"]["path"]["description"], "File path");
    // maxOutputTokens clamped to 65536.
    assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 65_536);
    assert_eq!(body["request"]["labels"]["model_enum"], "MODEL_PLACEHOLDER_M318");
    assert_eq!(body["request"]["labels"]["used_claude"], "false");
    assert!(body["request"]["labels"].get("last_execution_id").is_some());

    // 2. Another model's calls on Gemini 3+ (after a switch): real function
    // calls, the first of the step with Google's stand-in signature, and
    // their results as function responses (not text the model would imitate).
    let mut unsigned = req.clone();
    unsigned.messages[1] = json!({"role": "assistant", "content": [
        {"type": "reasoning", "text": "Claude's thought", "sig": "EqQB", "by": "claude"},
        {"type": "tool_call", "id": "c1", "name": "READ", "input": {"path": "a.rs"}},
        {"type": "tool_call", "id": "c2", "name": "READ", "input": {"path": "b.rs"}}
    ]});
    let ub = wire::request_body_for(&unsigned, "proj-1", "gemini-3.8-flash-high", 0, 1_700_000_000_000);
    let contents = &ub["request"]["contents"];
    let s = contents.to_string();
    assert!(!s.contains("Observation from") && !s.contains("Claude's thought"), "{s}");
    let calls = contents[1]["parts"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{s}");
    assert_eq!(calls[0]["functionCall"]["name"], "READ");
    assert_eq!(calls[0]["thoughtSignature"], "skip_thought_signature_validator");
    assert!(calls[1].get("thoughtSignature").is_none());
    assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "READ");

    // 3. Short system prompt (<= 126 chars) stays in systemInstruction; Claude uses legacy `parameters` allowlist + tool call IDs.
    let claude_req = Request {
        provider: Kind::Antigravity,
        model: "antigravity/claude-opus-4-6".into(),
        max_tokens: 64_000,
        system: "Short system prompt.".into(),
        thinking: true,
        effort: Some("high".into()),
        tools: vec![tool],
        tools_off: true,
        messages: vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_call", "id": "call:1/bad", "name": "READ", "input": {"path": "a.rs"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "image", "media_type": "image/png", "data": "AAAA"},
                {"type": "tool_result", "call_id": "call:1/bad", "content": "ok", "is_error": false}
            ]}),
        ],
        ..Default::default()
    };
    let cb = wire::request_body_for(&claude_req, "proj-1", "claude-opus-4-6-thinking", 0, 1_700_000_000_000);
    assert_eq!(cb["request"]["systemInstruction"]["parts"][0]["text"], "Short system prompt.");
    let cdecl = &cb["request"]["tools"][0]["functionDeclarations"][0];
    assert!(cdecl.get("parameters").is_some() && cdecl.get("parametersJsonSchema").is_none());
    assert!(cdecl["parameters"].get("additionalProperties").is_none());
    assert_eq!(cdecl["parameters"]["properties"]["path"]["type"], "string");
    assert_eq!(cb["request"]["toolConfig"]["functionCallingConfig"]["mode"], "NONE");
    assert_eq!(cb["request"]["labels"]["used_claude"], "true");
    // Claude functionResponse must come BEFORE inlineData image and have sanitized id.
    let c_user2 = &cb["request"]["contents"][2]["parts"];
    assert_eq!(c_user2[0]["functionResponse"]["id"], "call_1_bad");
    assert!(c_user2[1].get("inlineData").is_some());
}

#[test]
fn sse_translation_handles_thinking_text_tools_signatures_and_usage() {
    let mut t = wire::Translate::new("antigravity/gemini-3.8-flash");
    let mut all = Vec::new();
    for chunk in [
        json!({"response": {"responseId": "r1", "modelVersion": "gemini-3.8-flash-high", "candidates": [{"content": {"parts": [{"thought": true, "text": "Plan", "thoughtSignature": "U0lHMQ=="}]}}]}}),
        json!({"response": {"candidates": [{"content": {"parts": [{"text": "Done", "thoughtSignature": "U0lHMg=="}]}}]}}),
        json!({"response": {"candidates": [{"content": {"parts": [{"functionCall": {"id": "fc1", "name": "READ", "args": {"path": "x"}}, "thoughtSignature": "U0lHMw=="}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 1000, "cachedContentTokenCount": 600, "candidatesTokenCount": 40, "thoughtsTokenCount": 20}}}),
    ] {
        all.extend(t.chunk(&chunk));
    }
    all.extend(t.finish());

    let evs: Vec<Event> = all.iter().filter_map(|o| if let wire::Out::Event(e) = o { Some(e.clone()) } else { None }).collect();
    assert_eq!(evs[0], Event::Start { id: "r1".into(), model: "gemini-3.8-flash-high".into() });
    assert!(matches!(&evs[1], Event::Block { index: 0, block } if block["type"] == "reasoning" && block["by"] == "antigravity"));
    assert_eq!(evs[2], Event::Reasoning { index: 0, text: "Plan".into() });
    assert_eq!(evs[3], Event::Sig { index: 0, sig: "U0lHMQ==".into() });
    assert_eq!(evs[4], Event::BlockEnd { index: 0 });
    assert!(matches!(&evs[5], Event::Block { index: 1, block } if block["type"] == "text" && block["by"] == "antigravity"));
    assert_eq!(evs[6], Event::Text { index: 1, text: "Done".into() });
    assert_eq!(evs[7], Event::Sig { index: 1, sig: "U0lHMg==".into() });
    assert_eq!(evs[8], Event::BlockEnd { index: 1 });
    assert!(matches!(&evs[9], Event::Block { index: 2, block } if block["type"] == "tool_call" && block["id"] == "fc1" && block["sig"] == "U0lHMw=="));
    assert_eq!(evs[10], Event::Input { index: 2, json: "{\"path\":\"x\"}".into() });
    assert_eq!(evs[11], Event::BlockEnd { index: 2 });
    assert_eq!(evs[12], Event::Usage(Usage { input: 400, cache_read: 600, output: 60, reasoning: Some(20), ..Default::default() }));
    assert_eq!(evs[13], Event::Stop { reason: oak::stop::TOOL_CALLS.into(), detail: None });
    assert_eq!(all.last(), Some(&wire::Out::Done));
}

#[test]
fn quota_parsing_and_429_classification() {
    let summary = json!({
        "groups": [
            {
                "displayName": "Gemini Models",
                "description": "Models within this group: Gemini 4 Neon, Gemini 4 Hydrogen, Gemini 3.8 Flash",
                "buckets": [
                    {"bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly", "remainingFraction": 0.78, "resetTime": "2026-11-30T00:38:17Z"},
                    {"bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining", "window": "5h", "remainingFraction": 0.27, "resetTime": "2026-11-23T05:38:17Z"}
                ]
            },
            {
                "displayName": "Claude and GPT models",
                "description": "Models within this group: Claude Opus 5.5, Claude Sonnet 5.5, GPT-OSS 120B",
                "buckets": [
                    {"bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly", "remainingFraction": 0.0, "resetTime": "2026-11-30T01:51:17Z"},
                    {"bucketId": "3p-5h", "displayName": "Five Hour Limit Remaining", "window": "5h", "remainingFraction": 0.9, "resetTime": "2026-11-23T06:51:17Z"}
                ]
            },
            {
                "displayName": "Gemini 4 Argon",
                "description": "For paid Pro plans, Argon quota is separate.",
                "buckets": [
                    {"bucketId": "gemini-argon-wtu-weekly", "displayName": "Gemini 4 Argon Weekly Limit", "window": "weekly", "remainingFraction": 1.0, "resetTime": "2026-11-30T01:51:17Z"}
                ]
            }
        ]
    });
    let pools = catalog::parse_quota_summary(&summary);
    assert_eq!(pools.len(), 3);
    assert_eq!((pools[0].id.as_str(), pools[0].name.as_str()), ("gemini", "Gemini Models"));
    assert_eq!(pools[0].limits.len(), 2);
    assert_eq!(pools[0].limits[0].label, "5h");
    assert!((pools[0].limits[0].percent - 73.0).abs() < 0.01);
    assert_eq!(pools[0].limits[1].label, "Week");
    assert!((pools[0].limits[1].percent - 22.0).abs() < 0.01);

    assert_eq!((pools[1].id.as_str(), pools[1].name.as_str()), ("3p", "Claude and GPT models"));
    assert_eq!(pools[1].limits.len(), 2);
    assert_eq!(pools[1].limits[0].label, "5h");
    assert_eq!(pools[1].limits[1].label, "Week");
    assert!((pools[1].limits[1].percent - 100.0).abs() < 0.01);

    assert_eq!((pools[2].id.as_str(), pools[2].name.as_str()), ("argon", "Gemini 4 Argon"));
    assert_eq!(pools[2].limits.len(), 1);
    assert_eq!(pools[2].limits[0].label, "Week");
    assert!((pools[2].limits[0].percent - 0.0).abs() < 0.01);

    // Model-to-pool routing: Pro (separate Argon) vs Ultra (Argon folded into Gemini).
    assert_eq!(catalog::pool_for_model(&pools, "antigravity/gemini-3.8-flash").unwrap().id, "gemini");
    assert_eq!(catalog::pool_for_model(&pools, "antigravity/claude-opus-5-5").unwrap().id, "3p");
    assert_eq!(catalog::pool_for_model(&pools, "antigravity/gpt-oss-120b").unwrap().id, "3p");
    assert_eq!(catalog::pool_for_model(&pools, "antigravity/gemini-4-argon").unwrap().id, "argon");
    let ultra_pools = pools[..2].to_vec();
    assert_eq!(catalog::pool_for_model(&ultra_pools, "antigravity/gemini-4-argon").unwrap().id, "gemini");

    // Exhausting Claude & GPT (weekly 100% + refusal) must not block Gemini or Argon on the same account.
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-22T18:00:00Z").unwrap().timestamp_millis();
    let mut refused = std::collections::HashMap::new();
    refused.insert("3p".into(), now + 3_600_000);
    let claude_cand = auth::candidate_for_test(pools.clone(), refused.clone(), "antigravity/claude-opus-5-5", now);
    let gemini_cand = auth::candidate_for_test(pools.clone(), refused.clone(), "antigravity/gemini-3.8-flash", now);
    let argon_cand = auth::candidate_for_test(pools.clone(), refused, "antigravity/gemini-4-argon", now);
    assert!(!claude_cand.usable);
    assert!(gemini_cand.usable);
    assert!((gemini_cand.left - 27.0).abs() < 0.05);
    assert!(argon_cand.usable);
    assert!((argon_cand.left - 100.0).abs() < 0.05);

    // Fallback to per-model quotaInfo
    let raw_models = json!({
        "gemini-3.8-flash-high": {"quotaInfo": {"remainingFraction": 0.6, "resetTime": "2026-11-23T00:00:00Z"}},
        "claude-opus-4-6-thinking": {"quotaInfo": {"remainingFraction": 0.1, "resetTime": "2026-11-23T02:00:00Z"}},
        "gemini-4-argon-high": {"quotaInfo": {"remainingFraction": 0.9, "resetTime": "2026-11-29T00:00:00Z"}}
    });
    let fb = catalog::parse_model_quotas(raw_models.as_object().unwrap());
    assert_eq!(fb.len(), 3);
    assert_eq!(fb[0].id, "gemini");
    assert!((fb[0].limits[0].percent - 40.0).abs() < 0.01);
    assert_eq!(fb[1].id, "3p");
    assert!((fb[1].limits[0].percent - 90.0).abs() < 0.01);
    assert_eq!(fb[2].id, "argon");
    assert!((fb[2].limits[0].percent - 10.0).abs() < 0.01);

    // 429 classification & reset duration parsing
    let quota_wall = "Individual quota reached. Resets in 2h 15m 30s.";
    assert!(wire::is_account_quota_429(quota_wall));
    assert!(!wire::is_generic_resource_exhausted(quota_wall));
    assert_eq!(wire::parse_reset_time_ms(quota_wall, 1_000), Some(1_000 + (2 * 3600 + 15 * 60 + 30) * 1000));

    let generic_429 = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Resource has been exhausted (e.g. check quota)."}}"#;
    assert!(!wire::is_account_quota_429(generic_429));
    assert!(wire::is_generic_resource_exhausted(generic_429));
    assert!(wire::is_capacity_failure(429, generic_429));
    assert!(wire::is_capacity_failure(503, "No capacity available"));
}
