//! Antigravity model catalog: static bootstrap defaults, dynamic discovery via
//! `v1internal:fetchAvailableModels`, thinking-tier grouping & routing,
//! `model_enum` resolution, thinking config, and quota/usage extraction.

use super::{PREFIX, endpoint_candidates, error_text, with_headers};
use crate::api::{self, Limit};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

/// Public Antigravity model metadata as Oak uses it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// Oak model id: `"antigravity/<public_id>"`.
    pub id: String,
    /// Human-readable name (without provider suffix; Oak's picker shows it under Antigravity).
    pub name: String,
    pub context: u64,
    pub max_output: u64,
    /// Effort levels advertised for this model (subset of `agent::EFFORTS`).
    pub efforts: Vec<String>,
    pub images: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Routing {
    #[serde(default)]
    pub(crate) off: Option<String>,
    #[serde(default)]
    pub(crate) minimal: Option<String>,
    #[serde(default)]
    pub(crate) low: Option<String>,
    #[serde(default)]
    pub(crate) medium: Option<String>,
    #[serde(default)]
    pub(crate) high: Option<String>,
    #[serde(default)]
    pub(crate) xhigh: Option<String>,
    #[serde(default)]
    pub(crate) default_id: String,
}

impl Routing {
    fn all_ids(&self) -> Vec<&str> {
        [
            self.off.as_deref(),
            self.minimal.as_deref(),
            self.low.as_deref(),
            self.medium.as_deref(),
            self.high.as_deref(),
            self.xhigh.as_deref(),
            (!self.default_id.is_empty()).then_some(self.default_id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn resolve(&self, effort: Option<&str>, fallback: &str) -> String {
        let pick = |opts: &[Option<&str>]| -> String {
            opts.iter()
                .copied()
                .flatten()
                .find(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| if self.default_id.is_empty() { fallback.to_string() } else { self.default_id.clone() })
        };
        match effort.map(str::trim) {
            None | Some("") | Some("off") | Some("none") => pick(&[
                self.off.as_deref(),
                self.minimal.as_deref(),
                self.low.as_deref(),
                Some(self.default_id.as_str()),
            ]),
            Some("minimal") => pick(&[
                self.minimal.as_deref(),
                self.low.as_deref(),
                self.medium.as_deref(),
                self.high.as_deref(),
                self.off.as_deref(),
            ]),
            Some("low") => pick(&[
                self.low.as_deref(),
                self.minimal.as_deref(),
                self.medium.as_deref(),
                self.high.as_deref(),
                self.off.as_deref(),
            ]),
            Some("medium") => pick(&[
                self.medium.as_deref(),
                self.low.as_deref(),
                self.high.as_deref(),
                self.minimal.as_deref(),
                self.off.as_deref(),
            ]),
            Some("high") => pick(&[
                self.high.as_deref(),
                self.medium.as_deref(),
                self.low.as_deref(),
                self.minimal.as_deref(),
                self.off.as_deref(),
            ]),
            Some("xhigh") | Some("max") | Some("ultra") => pick(&[
                self.xhigh.as_deref(),
                self.high.as_deref(),
                self.medium.as_deref(),
                self.low.as_deref(),
                self.minimal.as_deref(),
                self.off.as_deref(),
            ]),
            _ => pick(&[self.low.as_deref(), self.minimal.as_deref(), self.off.as_deref()]),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub(crate) models: Vec<ModelInfo>,
    /// Keyed by public model id (without `"antigravity/"`).
    pub(crate) routing: HashMap<String, Routing>,
    /// Wire runtime model id -> server `MODEL_PLACEHOLDER_*` enum.
    #[serde(default)]
    pub(crate) model_enums: HashMap<String, String>,
    /// True once hydrated from a live `fetchAvailableModels` response.
    #[serde(default)]
    pub(crate) dynamic: bool,
}

static CATALOG: LazyLock<Mutex<Catalog>> = LazyLock::new(|| Mutex::new(bootstrap_catalog()));

fn cache_path() -> std::path::PathBuf {
    crate::store::dir().join("antigravity-models.json")
}

/// Strips the `"antigravity/"` prefix from an Oak model id.
pub fn public_id(id: &str) -> &str {
    id.strip_prefix(PREFIX).unwrap_or(id)
}

/// Adds the `"antigravity/"` prefix if not already present.
pub(crate) fn oak_id(pub_id: &str) -> String {
    if pub_id.starts_with(PREFIX) { pub_id.to_string() } else { format!("{PREFIX}{pub_id}") }
}

/// Whether `id` is an Antigravity model id (`antigravity/<model>`).
pub fn serves(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// Static bootstrap catalog mirroring `pi-antigravity`'s defaults.
pub(crate) fn bootstrap_catalog() -> Catalog {
    let mk = |pub_id: &str, name: &str, context: u64, max_output: u64, efforts: &[&str], images: bool| ModelInfo {
        id: oak_id(pub_id),
        name: name.to_string(),
        context,
        max_output,
        efforts: efforts.iter().map(|s| s.to_string()).collect(),
        images,
    };
    let models = vec![
        mk("gemini-3.8-flash", "Gemini 3.8 Flash", 1_048_576, 65_536, &["low", "medium", "high"], true),
        mk("gemini-3.7-flash", "Gemini 3.7 Flash", 1_048_576, 65_536, &["low", "medium", "high"], true),
        mk("gemini-3.6-flash", "Gemini 3.6 Flash", 1_048_576, 65_536, &["low", "medium", "high"], true),
        mk("claude-opus-4-6", "Claude Opus 4.6", 250_000, 64_000, &["high"], true),
        mk("claude-sonnet-4-6", "Claude Sonnet 4.6", 200_000, 64_000, &["high"], true),
        mk("gemini-3.1-pro", "Gemini 3.1 Pro", 1_048_576, 65_535, &["low", "high"], true),
        mk("gemini-3.5-flash", "Gemini 3.5 Flash", 1_048_576, 65_536, &["low", "medium", "high"], true),
        mk("gpt-oss-120b", "GPT-OSS 120B", 131_072, 32_768, &["medium"], false),
    ];

    let mut routing = HashMap::new();
    let mut put = |pub_id: &str, r: Routing| {
        routing.insert(pub_id.to_string(), r);
    };
    let s = |v: &str| Some(v.to_string());
    put(
        "claude-opus-4-6",
        Routing {
            off: None,
            minimal: s("claude-opus-4-6-thinking"),
            low: s("claude-opus-4-6-thinking"),
            medium: s("claude-opus-4-6-thinking"),
            high: s("claude-opus-4-6-thinking"),
            xhigh: s("claude-opus-4-6-thinking"),
            default_id: "claude-opus-4-6-thinking".into(),
        },
    );
    put(
        "claude-sonnet-4-6",
        Routing {
            off: s("claude-sonnet-4-6"),
            minimal: s("claude-sonnet-4-6"),
            low: s("claude-sonnet-4-6"),
            medium: s("claude-sonnet-4-6"),
            high: s("claude-sonnet-4-6"),
            xhigh: s("claude-sonnet-4-6"),
            default_id: "claude-sonnet-4-6".into(),
        },
    );
    put(
        "gemini-3.1-pro",
        Routing {
            off: s("gemini-3.1-pro-low"),
            minimal: s("gemini-3.1-pro-low"),
            low: s("gemini-3.1-pro-low"),
            medium: s("gemini-3.1-pro-low"),
            high: s("gemini-pro-agent"),
            xhigh: s("gemini-pro-agent"),
            default_id: "gemini-3.1-pro-low".into(),
        },
    );
    for ver in ["3.8", "3.7", "3.6"] {
        let base = format!("gemini-{ver}-flash");
        put(
            &base,
            Routing {
                off: s(&format!("{base}-low")),
                minimal: s(&format!("{base}-low")),
                low: s(&format!("{base}-low")),
                medium: s(&format!("{base}-medium")),
                high: s(&format!("{base}-high")),
                xhigh: s(&format!("{base}-high")),
                default_id: format!("{base}-low"),
            },
        );
    }
    put(
        "gemini-3.5-flash",
        Routing {
            off: s("gemini-3.5-flash-extra-low"),
            minimal: s("gemini-3.5-flash-extra-low"),
            low: s("gemini-3.5-flash-extra-low"),
            medium: s("gemini-3.5-flash-low"),
            high: s("gemini-3-flash-agent"),
            xhigh: s("gemini-3-flash-agent"),
            default_id: "gemini-3.5-flash-extra-low".into(),
        },
    );
    put(
        "gpt-oss-120b",
        Routing {
            off: s("gpt-oss-120b-medium"),
            minimal: s("gpt-oss-120b-medium"),
            low: s("gpt-oss-120b-medium"),
            medium: s("gpt-oss-120b-medium"),
            high: s("gpt-oss-120b-medium"),
            xhigh: s("gpt-oss-120b-medium"),
            default_id: "gpt-oss-120b-medium".into(),
        },
    );

    Catalog { models, routing, model_enums: HashMap::new(), dynamic: false }
}

/// Restores a previously cached catalog from `antigravity-models.json`.
pub(crate) fn load_cached() {
    if let Some(cat) = crate::store::read_json::<Catalog>(&cache_path())
        && !cat.models.is_empty()
    {
        *CATALOG.lock().unwrap() = cat;
    }
}

fn save_cached(cat: &Catalog) {
    if !cfg!(test) {
        let _ = crate::store::write_json(&cache_path(), cat);
    }
}

/// Current selectable Antigravity models (Oak ids: `"antigravity/<public_id>"`).
pub fn models() -> Vec<ModelInfo> {
    CATALOG.lock().unwrap().models.clone()
}

/// Looks up model metadata by Oak id (`"antigravity/..."`) or public id.
pub fn model_info(id: &str) -> Option<ModelInfo> {
    let want = oak_id(public_id(id));
    CATALOG.lock().unwrap().models.iter().find(|m| m.id == want).cloned()
}

/// Human-readable display name for an Antigravity model id.
pub fn model_name(id: &str) -> String {
    model_info(id).map_or_else(|| humanize_public_id(public_id(id)), |m| m.name)
}

/// Context window in tokens for an Antigravity model id.
pub fn context_window(id: &str) -> Option<u64> {
    if let Some(m) = model_info(id) {
        return Some(m.context);
    }
    let pub_id = public_id(id);
    if pub_id.starts_with("gemini-") {
        Some(1_048_576)
    } else if pub_id.starts_with("claude-opus") {
        Some(250_000)
    } else if pub_id.starts_with("claude-") {
        Some(200_000)
    } else if pub_id.starts_with("gpt-oss") {
        Some(131_072)
    } else {
        Some(128_000)
    }
}

/// Effort levels accepted by `id` (subset of `crate::agent::EFFORTS`).
pub fn efforts(id: &str) -> Vec<&'static str> {
    if let Some(m) = model_info(id) {
        return crate::agent::EFFORTS
            .iter()
            .copied()
            .filter(|e| m.efforts.iter().any(|l| l == e))
            .collect();
    }
    let pub_id = public_id(id);
    if pub_id.starts_with("gpt-oss") {
        vec!["medium"]
    } else if pub_id.starts_with("claude-opus-4") || pub_id.starts_with("claude-sonnet-4") {
        vec!["high"]
    } else if pub_id.starts_with("gemini-3.1-pro") {
        vec!["low", "high"]
    } else {
        vec!["low", "medium", "high"]
    }
}

/// Static verified max output tokens per model / runtime id (from `pi-antigravity`).
pub(crate) fn static_max_output(id: &str) -> Option<u64> {
    Some(match id {
        "gemini-4-neon" | "gemini-4-neon-low" | "gemini-4-neon-medium" | "gemini-4-neon-high" | "gemini-4-neon-xhigh" => 262_144,
        "gemini-4-argon" | "gemini-4-argon-low" | "gemini-4-argon-medium" | "gemini-4-argon-high" => 65_536,
        "gemini-4-hydrogen" | "gemini-4-hydrogen-low" | "gemini-4-hydrogen-medium" | "gemini-4-hydrogen-high" | "gemini-4-hydrogen-xhigh" => 262_144,
        "claude-opus-5-5" | "claude-opus-5-5-low" | "claude-opus-5-5-medium" | "claude-opus-5-5-high" => 128_000,
        "claude-sonnet-5-5" | "claude-sonnet-5-5-low" | "claude-sonnet-5-5-medium" | "claude-sonnet-5-5-high" => 128_000,
        "gemini-3.8-flash" | "gemini-3.8-flash-low" | "gemini-3.8-flash-medium" | "gemini-3.8-flash-high" => 65_536,
        "gemini-3.7-flash" | "gemini-3.7-flash-tiered" | "gemini-3.7-flash-low" | "gemini-3.7-flash-medium" | "gemini-3.7-flash-high" => 65_536,
        "gemini-3.6-flash" | "gemini-3.6-flash-low" | "gemini-3.6-flash-medium" | "gemini-3.6-flash-high" => 65_536,
        "gemini-3.5-flash" | "gemini-3.5-flash-extra-low" | "gemini-3.5-flash-low" | "gemini-3-flash-agent" => 65_536,
        "gemini-3.1-pro" | "gemini-3.1-pro-low" | "gemini-3.1-pro-high" | "gemini-pro-agent" => 65_535,
        "claude-opus-4-6" | "claude-opus-4-6-thinking" | "claude-sonnet-4-6" => 64_000,
        "gpt-oss-120b" | "gpt-oss-120b-medium" => 32_768,
        _ => return None,
    })
}

/// Maximum output tokens accepted by the backend for `model_id` (and optional `runtime_model`).
pub fn max_output(model_id: &str) -> u64 {
    max_output_for(model_id, None)
}

pub(crate) fn max_output_for(model_id: &str, runtime_model: Option<&str>) -> u64 {
    let pub_id = public_id(model_id);
    {
        let cat = CATALOG.lock().unwrap();
        if cat.dynamic
            && let Some(m) = cat.models.iter().find(|m| public_id(&m.id) == pub_id)
        {
            return m.max_output;
        }
    }
    if let Some(rt) = runtime_model
        && let Some(n) = static_max_output(rt)
    {
        return n;
    }
    if let Some(n) = static_max_output(pub_id) {
        return n;
    }
    let check = runtime_model.unwrap_or(pub_id);
    if check.starts_with("claude-opus-5-") || check.starts_with("claude-sonnet-5-") {
        128_000
    } else if check.starts_with("claude-") {
        64_000
    } else if check.starts_with("gpt-oss-") {
        32_768
    } else if check.starts_with("gemini-4-neon") || check.starts_with("gemini-4-hydrogen") {
        262_144
    } else if check.starts_with("gemini-3.1-pro") || check == "gemini-pro-agent" {
        65_535
    } else if check.starts_with("gemini-") {
        65_536
    } else {
        8_192
    }
}

/// Static fallback `model_enum` mapping (from `pi-antigravity`).
fn static_model_enum(wire_id: &str) -> Option<&'static str> {
    Some(match wire_id {
        "gemini-4-neon" | "gemini-4-neon-low" => "MODEL_PLACEHOLDER_M534",
        "gemini-4-neon-high" => "MODEL_PLACEHOLDER_M532",
        "gemini-4-neon-xhigh" => "MODEL_PLACEHOLDER_M535",
        "gemini-4-neon-medium" => "MODEL_PLACEHOLDER_M533",
        "gemini-4-argon" | "gemini-4-argon-low" => "MODEL_PLACEHOLDER_M419",
        "gemini-4-argon-high" => "MODEL_PLACEHOLDER_M417",
        "gemini-4-argon-medium" => "MODEL_PLACEHOLDER_M418",
        "gemini-4-hydrogen" | "gemini-4-hydrogen-low" => "MODEL_PLACEHOLDER_M527",
        "gemini-4-hydrogen-high" => "MODEL_PLACEHOLDER_M525",
        "gemini-4-hydrogen-xhigh" => "MODEL_PLACEHOLDER_M528",
        "gemini-4-hydrogen-medium" => "MODEL_PLACEHOLDER_M526",
        "claude-opus-5-5" | "claude-opus-5-5-low" => "MODEL_PLACEHOLDER_M400",
        "claude-opus-5-5-medium" => "MODEL_PLACEHOLDER_M401",
        "claude-opus-5-5-high" => "MODEL_PLACEHOLDER_M402",
        "claude-sonnet-5-5" | "claude-sonnet-5-5-low" => "MODEL_PLACEHOLDER_M403",
        "claude-sonnet-5-5-medium" => "MODEL_PLACEHOLDER_M404",
        "claude-sonnet-5-5-high" => "MODEL_PLACEHOLDER_M405",
        "gemini-3.8-flash" | "gemini-3.8-flash-high" => "MODEL_PLACEHOLDER_M318",
        "gemini-3.8-flash-medium" => "MODEL_PLACEHOLDER_M319",
        "gemini-3.8-flash-low" => "MODEL_PLACEHOLDER_M320",
        "gemini-3.8-flash-tiered" => "MODEL_PLACEHOLDER_M322",
        "gemini-3.7-flash" | "gemini-3.7-flash-high" => "MODEL_PLACEHOLDER_M298",
        "gemini-3.7-flash-medium" => "MODEL_PLACEHOLDER_M299",
        "gemini-3.7-flash-low" => "MODEL_PLACEHOLDER_M300",
        "gemini-3.7-flash-tiered" => "MODEL_PLACEHOLDER_M301",
        "gemini-3.6-flash" | "gemini-3.6-flash-high" => "MODEL_PLACEHOLDER_M71",
        "gemini-3.6-flash-medium" => "MODEL_PLACEHOLDER_M72",
        "gemini-3.6-flash-low" => "MODEL_PLACEHOLDER_M73",
        "gemini-3.6-flash-tiered" => "MODEL_PLACEHOLDER_M196",
        "gemini-3.5-flash" | "gemini-3.5-flash-low" => "MODEL_PLACEHOLDER_M20",
        "gemini-3.5-flash-extra-low" => "MODEL_PLACEHOLDER_M187",
        "gemini-3-flash-agent" => "MODEL_PLACEHOLDER_M84",
        "gemini-3.1-pro" | "gemini-3.1-pro-low" => "MODEL_PLACEHOLDER_M36",
        "gemini-3.1-pro-high" => "MODEL_PLACEHOLDER_M37",
        "gemini-pro-agent" => "MODEL_PLACEHOLDER_M16",
        "claude-sonnet-4-6" => "MODEL_PLACEHOLDER_M35",
        "claude-opus-4-6" | "claude-opus-4-6-thinking" => "MODEL_PLACEHOLDER_M26",
        "gpt-oss-120b" | "gpt-oss-120b-medium" => "MODEL_OPENAI_GPT_OSS_120B_MEDIUM",
        _ => return None,
    })
}

pub(crate) fn model_enum(wire_id: &str) -> Option<String> {
    let cat = CATALOG.lock().unwrap();
    if let Some(e) = cat.model_enums.get(wire_id) {
        return Some(e.clone());
    }
    if let Some(e) = static_model_enum(wire_id) {
        return Some(e.to_string());
    }
    if let Some(r) = cat.routing.get(wire_id) {
        let routed = r.resolve(None, wire_id);
        if let Some(e) = cat.model_enums.get(&routed) {
            return Some(e.clone());
        }
        if let Some(e) = static_model_enum(&routed) {
            return Some(e.to_string());
        }
    }
    None
}

/// Resolves an Oak/public model id + requested effort into a backend runtime model id.
pub(crate) fn request_model_id(model_id: &str, effort: Option<&str>) -> String {
    let pub_id = public_id(model_id);
    let cat = CATALOG.lock().unwrap();
    match cat.routing.get(pub_id) {
        Some(r) => r.resolve(effort, pub_id),
        None => pub_id.to_string(),
    }
}

pub(crate) fn is_known_model(model_id: &str) -> bool {
    CATALOG.lock().unwrap().routing.contains_key(public_id(model_id))
}

/// Legacy Flash downgrade candidate when a newer generation isn't yet rolled out.
pub(crate) fn fallback_runtime_model(runtime_model: &str, effort: Option<&str>) -> Option<String> {
    let cat = CATALOG.lock().unwrap();
    let candidate = if let Some(rest) = runtime_model.strip_prefix("gemini-3.8-flash-") {
        Some(format!("gemini-3.7-flash-{rest}"))
    } else if runtime_model == "gemini-3.8-flash" {
        Some("gemini-3.7-flash-low".to_string())
    } else if runtime_model == "gemini-3.7-flash-tiered" {
        cat.routing.get("gemini-3.6-flash").map(|r| r.resolve(effort, "gemini-3.6-flash"))
    } else if let Some(rest) = runtime_model.strip_prefix("gemini-3.7-flash-") {
        Some(format!("gemini-3.6-flash-{rest}"))
    } else if runtime_model == "gemini-3.7-flash" {
        Some("gemini-3.6-flash-low".to_string())
    } else {
        None
    }?;
    if !cat.dynamic {
        return Some(candidate);
    }
    let advertised = cat.routing.values().any(|r| r.all_ids().contains(&candidate.as_str()));
    advertised.then_some(candidate)
}

const RUNTIME_TIER_SUFFIXES: &[&str] = &["-extra-low", "-low", "-medium", "-high", "-max", "-xhigh"];

fn split_runtime_tier(runtime_model: &str) -> Option<(&str, &str)> {
    for &suffix in RUNTIME_TIER_SUFFIXES {
        if let Some(base) = runtime_model.strip_suffix(suffix)
            && !base.is_empty()
        {
            return Some((base, suffix));
        }
    }
    None
}

/// Same-generation sibling tiers to try when one tier returns a generic 429 capacity error.
pub(crate) fn advertised_sibling_runtimes(runtime_model: &str) -> Vec<String> {
    let Some((base, cur_suffix)) = split_runtime_tier(runtime_model) else { return vec![] };
    let preferred = ["-high", "-medium", "-xhigh", "-low", "-extra-low", "-max"];
    let candidates: Vec<String> = preferred
        .iter()
        .copied()
        .filter(|&s| s != cur_suffix)
        .map(|s| format!("{base}{s}"))
        .collect();

    let cat = CATALOG.lock().unwrap();
    let mut known: HashSet<&str> = HashSet::new();
    for r in cat.routing.values() {
        for id in r.all_ids() {
            known.insert(id);
        }
    }
    let routed: Vec<String> = candidates.iter().filter(|id| known.contains(id.as_str())).cloned().collect();
    if !routed.is_empty() {
        return routed;
    }
    if base.starts_with("gemini-") || base.starts_with("claude-") {
        return candidates.into_iter().take(2).collect();
    }
    vec![]
}

/// `generationConfig.thinkingConfig` payload for `runtime_model` and `effort`.
pub(crate) fn thinking_config(runtime_model: &str, effort: Option<&str>) -> Option<Value> {
    let off = matches!(effort, None | Some("") | Some("off") | Some("none"));
    let eff = effort.unwrap_or("off");
    if runtime_model.starts_with("claude-") {
        return Some(if off {
            json!({"includeThoughts": false, "thinkingBudget": 0})
        } else {
            json!({"includeThoughts": true, "thinkingBudget": 1024})
        });
    }
    if runtime_model.starts_with("gpt-oss-") {
        return Some(if off {
            json!({"includeThoughts": false, "thinkingBudget": 0})
        } else {
            json!({"includeThoughts": true, "thinkingBudget": 8192})
        });
    }
    if runtime_model.starts_with("gemini-3.5-flash") || runtime_model == "gemini-3-flash-agent" {
        if off {
            return Some(json!({"includeThoughts": false, "thinkingBudget": 0}));
        }
        let budget = match eff {
            "high" | "xhigh" | "max" | "ultra" => 10_000,
            "medium" => 4_000,
            _ => 1_000,
        };
        return Some(json!({"includeThoughts": true, "thinkingBudget": budget}));
    }
    if runtime_model.starts_with("gemini-3.1-pro") || runtime_model == "gemini-pro-agent" {
        if off {
            return Some(json!({"includeThoughts": false, "thinkingBudget": 0}));
        }
        let budget = match eff {
            "high" | "xhigh" | "max" | "ultra" => 10_001,
            _ => 1_001,
        };
        return Some(json!({"includeThoughts": true, "thinkingBudget": budget}));
    }
    // Gemini 4 / Argon rejects thinkingBudget 0 and anything below 1000.
    if runtime_model.contains("argon") || runtime_model.starts_with("gemini-4") {
        if off {
            return None;
        }
        let budget = match eff {
            "high" | "xhigh" | "max" | "ultra" => -1,
            "medium" => 4_000,
            _ => 1_000,
        };
        return Some(json!({"includeThoughts": true, "thinkingBudget": budget}));
    }
    if runtime_model.starts_with("gemini-") {
        if off {
            return Some(json!({"includeThoughts": false, "thinkingBudget": 0}));
        }
        let budget = match eff {
            "high" | "xhigh" | "max" | "ultra" => -1,
            "medium" => 4_000,
            _ => 1_000,
        };
        return Some(json!({"includeThoughts": true, "thinkingBudget": budget}));
    }
    None
}

// ---------------------------------------------------------------- dynamic catalog discovery & grouping

pub(crate) fn is_usable_runtime_model_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return false;
    }
    let lower = id.to_ascii_lowercase();
    !lower.starts_with("model_") && !lower.starts_with("chat_") && !lower.starts_with("tab_") && !lower.contains("image")
}

fn is_selectable_runtime_model(id: &str, info: &Value) -> bool {
    if !is_usable_runtime_model_id(id) {
        return false;
    }
    if id.to_ascii_lowercase().ends_with("-disabled") {
        return false;
    }
    if info["isInternal"].as_bool() == Some(true)
        || info["disabled"].as_bool() == Some(true)
        || info["apiProvider"].as_str() == Some("API_PROVIDER_INTERNAL")
    {
        return false;
    }
    if let Some(dn) = raw_display_name(info)
        && dn.to_ascii_lowercase().contains("image")
    {
        return false;
    }
    true
}

fn raw_display_name(info: &Value) -> Option<&str> {
    info["displayName"]
        .as_str()
        .or_else(|| info["label"].as_str())
        .or_else(|| info["modelName"].as_str())
        .filter(|s| !s.is_empty())
}

fn is_extra_allowed_model_id(id: &str) -> bool {
    id.to_ascii_lowercase()
        .split('-')
        .any(|tok| matches!(tok, "neon" | "hydrogen" | "oxygen"))
}

fn extract_recommended_ids(agent_model_sorts: Option<&Value>) -> Option<HashSet<String>> {
    let arr = agent_model_sorts?.as_array()?;
    let mut ids = HashSet::new();
    for sort in arr {
        for group in sort["groups"].as_array().into_iter().flatten() {
            for id in group["modelIds"].as_array().into_iter().flatten() {
                if let Some(s) = id.as_str().filter(|s| !s.is_empty()) {
                    ids.insert(s.to_string());
                }
            }
        }
    }
    (!ids.is_empty()).then_some(ids)
}

#[derive(Default)]
struct RuntimeGroup {
    public_id: String,
    variants: HashMap<&'static str, String>,
    unsuffixed: Option<String>,
    display_names: Vec<String>,
    supports_thinking: Option<bool>,
    supports_images: Option<bool>,
    context_window: Option<u64>,
    max_output: Option<u64>,
}

const THINKING_SUFFIXES: &[(&str, &str)] = &[
    ("extra-low", "low"),
    ("extra-high", "xhigh"),
    ("xhigh", "xhigh"),
    ("thinking", "high"),
    ("minimal", "minimal"),
    ("medium", "medium"),
    ("high", "high"),
    ("low", "low"),
];

fn parse_thinking_suffix(runtime_id: &str) -> Option<(&str, &'static str)> {
    let lower = runtime_id.to_ascii_lowercase();
    for &(suffix, level) in THINKING_SUFFIXES {
        let needle = format!("-{suffix}");
        if lower.ends_with(&needle) && runtime_id.len() > needle.len() {
            return Some((&runtime_id[..runtime_id.len() - needle.len()], level));
        }
    }
    None
}

fn level_from_display_name(name: &str) -> Option<&'static str> {
    let start = name.rfind('(')?;
    let end = name[start..].find(')')?;
    let inner = name[start + 1..start + end].trim().to_ascii_lowercase();
    let compact: String = inner.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    match compact.as_str() {
        "extralow" => Some("low"),
        "extrahigh" | "xhigh" => Some("xhigh"),
        "thinking" => Some("high"),
        "minimal" => Some("minimal"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "low" => Some("low"),
        _ => None,
    }
}

fn strip_level_parens(name: &str) -> String {
    if let Some(start) = name.rfind('(')
        && let Some(rel_end) = name[start..].find(')')
        && name[start + rel_end + 1..].trim().is_empty()
        && level_from_display_name(&name[start..]).is_some()
    {
        return name[..start].trim().to_string();
    }
    name.trim().to_string()
}

fn display_family(name: &str) -> String {
    strip_level_parens(name).to_ascii_lowercase()
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut new_word = true;
    for ch in s.chars() {
        if new_word && ch.is_ascii_alphabetic() {
            out.push(ch.to_ascii_uppercase());
            new_word = false;
        } else {
            out.push(ch);
            new_word = !ch.is_ascii_alphanumeric();
        }
    }
    out
}

pub(crate) fn humanize_public_id(id: &str) -> String {
    let tokens: Vec<&str> = id.split('-').filter(|t| !t.is_empty()).collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        let next = tokens.get(i + 1).copied();
        if tok.eq_ignore_ascii_case("gpt") && next.is_some_and(|n| n.eq_ignore_ascii_case("oss")) {
            words.push("GPT-OSS".to_string());
            i += 2;
            continue;
        }
        if tok.chars().all(|c| c.is_ascii_digit())
            && let Some(n) = next
            && n.chars().all(|c| c.is_ascii_digit())
        {
            words.push(format!("{tok}.{n}"));
            i += 2;
            continue;
        }
        if tok.starts_with(|c: char| c.is_ascii_digit()) {
            words.push(tok.to_ascii_uppercase());
            i += 1;
            continue;
        }
        let mut chars = tok.chars();
        let w = chars.next().map_or_else(String::new, |f| f.to_ascii_uppercase().to_string() + chars.as_str());
        words.push(w);
        i += 1;
    }
    words.join(" ")
}

fn absorb_metadata(group: &mut RuntimeGroup, info: &Value, display_name: Option<&str>) {
    if let Some(dn) = display_name {
        group.display_names.push(dn.to_string());
    }
    if let Some(ctx) = info["maxTokens"].as_u64().filter(|&n| n > 0) {
        group.context_window = Some(group.context_window.map_or(ctx, |old| old.min(ctx)));
    }
    if let Some(out) = info["maxOutputTokens"].as_u64().filter(|&n| n > 0) {
        group.max_output = Some(group.max_output.map_or(out, |old| old.min(out)));
    }
    match info["supportsThinking"].as_bool() {
        Some(true) => group.supports_thinking = Some(true),
        Some(false) if group.supports_thinking != Some(true) => group.supports_thinking = Some(false),
        _ => {}
    }
    match info["supportsImages"].as_bool() {
        Some(false) => group.supports_images = Some(false),
        Some(true) if group.supports_images.is_none() => group.supports_images = Some(true),
        _ => {}
    }
}

fn merge_agent_singletons(groups: &mut BTreeMap<String, RuntimeGroup>) {
    let keys: Vec<String> = groups.keys().cloned().collect();
    for pub_id in keys {
        if !pub_id.ends_with("-agent") {
            continue;
        }
        let Some(g) = groups.get(&pub_id) else { continue };
        if g.supports_thinking == Some(false) {
            continue;
        }
        let mut ids: Vec<String> = g.unsuffixed.iter().cloned().collect();
        ids.extend(g.variants.values().cloned());
        if ids.len() != 1 {
            continue;
        }
        let runtime_id = ids.remove(0);
        let Some(first_name) = g.display_names.first().cloned() else { continue };
        let family = display_family(&first_name);
        if family.is_empty() {
            continue;
        }
        let level = g
            .variants
            .iter()
            .find(|(_, v)| *v == &runtime_id)
            .map(|(k, _)| *k)
            .or_else(|| level_from_display_name(&first_name));
        let Some(level) = level else { continue };

        let target_key = groups
            .iter()
            .find(|(k, cand)| *k != &pub_id && cand.display_names.iter().any(|n| display_family(n) == family))
            .map(|(k, _)| k.clone());
        let Some(target_key) = target_key else { continue };
        let can_merge = groups.get(&target_key).is_some_and(|t| !t.variants.contains_key(level));
        if !can_merge {
            continue;
        }
        let removed = groups.remove(&pub_id).unwrap();
        let target = groups.get_mut(&target_key).unwrap();
        target.variants.insert(level, runtime_id);
        let synthetic = json!({
            "supportsThinking": removed.supports_thinking,
            "supportsImages": removed.supports_images,
            "maxTokens": removed.context_window,
            "maxOutputTokens": removed.max_output,
        });
        absorb_metadata(target, &synthetic, removed.display_names.first().map(String::as_str));
    }
}

fn parse_gemini_version(id: &str) -> i32 {
    let Some(rest) = id.strip_prefix("gemini-") else { return 0 };
    let mut parts = rest.split(['.', '-']);
    let major = parts.next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
    major * 1000 + minor
}

fn model_rank(pub_id: &str) -> (u8, i32) {
    let lower = pub_id.to_ascii_lowercase();
    let ver = parse_gemini_version(&lower);
    if lower.starts_with("gemini-") && lower.contains("flash") && !lower.contains("pro") {
        (0, -ver)
    } else if lower.starts_with("claude-opus") {
        (1, 0)
    } else if lower.starts_with("claude-sonnet") {
        (2, 0)
    } else if lower.starts_with("claude-") {
        (3, 0)
    } else if lower.starts_with("gemini-") && lower.contains("pro") {
        (4, -ver)
    } else if lower.starts_with("gemini-") {
        (5, -ver)
    } else if lower.starts_with("gpt-oss") {
        (6, 0)
    } else {
        (7, 0)
    }
}

fn family_template<'a>(pub_id: &str, fallback: &'a [ModelInfo]) -> Option<&'a ModelInfo> {
    let lower = pub_id.to_ascii_lowercase();
    let find = |pred: &dyn Fn(&str) -> bool| fallback.iter().find(|m| pred(public_id(&m.id)));
    if lower.starts_with("gemini-") && lower.contains("flash") {
        find(&|id| id.starts_with("gemini-") && id.contains("flash"))
    } else if lower.starts_with("gemini-") && lower.contains("pro") {
        find(&|id| id.starts_with("gemini-") && id.contains("pro"))
    } else if lower.starts_with("claude-opus") {
        find(&|id| id.starts_with("claude-opus"))
    } else if lower.starts_with("claude-") {
        find(&|id| id.starts_with("claude-sonnet")).or_else(|| find(&|id| id.starts_with("claude-")))
    } else if lower.starts_with("gpt-oss") {
        find(&|id| id.starts_with("gpt-oss"))
    } else if lower.starts_with("gemini-") {
        find(&|id| id.starts_with("gemini-"))
    } else {
        None
    }
}

/// Builds a grouped `Catalog` from a merged `fetchAvailableModels` `models` map
/// and optional `agentModelSorts`.
pub(crate) fn build_catalog(
    raw_models: &serde_json::Map<String, Value>,
    agent_model_sorts: Option<&Value>,
) -> Result<Catalog, String> {
    let fallback = bootstrap_catalog();
    let recommended = extract_recommended_ids(agent_model_sorts);

    let collect = |filter: Option<&HashSet<String>>| -> BTreeMap<String, RuntimeGroup> {
        let mut map: BTreeMap<String, RuntimeGroup> = BTreeMap::new();
        for (runtime_id, info) in raw_models {
            if !is_selectable_runtime_model(runtime_id, info) {
                continue;
            }
            if let Some(ids) = filter
                && !ids.contains(runtime_id)
                && !is_extra_allowed_model_id(runtime_id)
            {
                continue;
            }
            let dn = raw_display_name(info);
            if let Some(base_id) = runtime_id.strip_suffix("-tiered") {
                let g = map.entry(base_id.to_string()).or_insert_with(|| RuntimeGroup { public_id: base_id.to_string(), ..Default::default() });
                absorb_metadata(g, info, dn);
                if g.unsuffixed.is_none() {
                    g.unsuffixed = Some(runtime_id.clone());
                }
                continue;
            }
            let suffix = parse_thinking_suffix(runtime_id);
            let pub_id = suffix.map_or(runtime_id.as_str(), |(b, _)| b).to_string();
            let g = map.entry(pub_id.clone()).or_insert_with(|| RuntimeGroup { public_id: pub_id, ..Default::default() });
            absorb_metadata(g, info, dn);
            let level = dn.and_then(level_from_display_name).or_else(|| suffix.map(|(_, l)| l));
            if let Some(lvl) = level {
                g.variants.insert(lvl, runtime_id.clone());
            } else {
                g.unsuffixed = Some(runtime_id.clone());
            }
        }
        merge_agent_singletons(&mut map);
        map
    };

    let mut groups = collect(recommended.as_ref());
    if groups.is_empty() && recommended.is_some() {
        groups = collect(None);
    }
    if groups.is_empty() {
        return Err("Antigravity catalog contains no supported chat models".into());
    }

    let mut model_enums = HashMap::new();
    for (wire_id, info) in raw_models {
        if let Some(e) = info["model"].as_str().filter(|s| !s.is_empty()) {
            model_enums.insert(wire_id.clone(), e.to_string());
        }
    }

    let mut models = Vec::new();
    let mut routing = HashMap::new();
    for g in groups.values() {
        let tmpl = family_template(&g.public_id, &fallback.models);
        let default_id = ["low", "minimal", "medium", "high", "xhigh"]
            .iter()
            .find_map(|k| g.variants.get(k).cloned())
            .or_else(|| g.unsuffixed.clone())
            .unwrap_or_else(|| g.public_id.clone());
        let pick = |keys: &[&str]| -> Option<String> {
            keys.iter()
                .find_map(|k| g.variants.get(k).cloned())
                .or_else(|| g.unsuffixed.clone())
                .or_else(|| Some(default_id.clone()))
        };
        let route = Routing {
            off: pick(&["low", "minimal", "medium", "high", "xhigh"]),
            minimal: pick(&["minimal", "low", "medium", "high", "xhigh"]),
            low: pick(&["low", "minimal", "medium", "high", "xhigh"]),
            medium: pick(&["medium", "low", "high", "minimal", "xhigh"]),
            high: pick(&["high", "medium", "low", "minimal", "xhigh"]),
            xhigh: pick(&["xhigh", "high", "medium", "low"]),
            default_id: default_id.clone(),
        };

        // Effort levels mapped to Oak's EFFORTS ("low", "medium", "high", "xhigh").
        let mut effs: Vec<String> = Vec::new();
        if g.supports_thinking != Some(false) {
            let has = |k: &str| g.variants.contains_key(k);
            if has("low") || has("minimal") {
                effs.push("low".into());
            }
            if has("medium") {
                effs.push("medium".into());
            }
            if has("high") {
                effs.push("high".into());
            }
            if has("xhigh") {
                effs.push("xhigh".into());
            }
            if effs.is_empty() && (g.supports_thinking == Some(true) || (g.supports_thinking.is_none() && tmpl.is_some_and(|t| !t.efforts.is_empty()))) {
                effs.push("high".into());
            }
        }

        let name = g
            .display_names
            .iter()
            .map(|n| strip_level_parens(n))
            .find(|n| !n.is_empty())
            .map(|n| if n.chars().all(|c| !c.is_ascii_uppercase()) { title_case(&n) } else { n })
            .unwrap_or_else(|| humanize_public_id(&g.public_id));
        let context = g.context_window.or_else(|| tmpl.map(|t| t.context)).unwrap_or(128_000);
        let max_output = g
            .max_output
            .or_else(|| static_max_output(&g.public_id))
            .or_else(|| tmpl.map(|t| t.max_output))
            .unwrap_or(8_192);
        let images = g.supports_images.or_else(|| tmpl.map(|t| t.images)).unwrap_or(true);

        models.push(ModelInfo {
            id: oak_id(&g.public_id),
            name,
            context,
            max_output,
            efforts: effs,
            images,
        });
        routing.insert(g.public_id.clone(), route);
    }

    models.sort_by(|a, b| {
        let ra = model_rank(public_id(&a.id));
        let rb = model_rank(public_id(&b.id));
        ra.cmp(&rb).then_with(|| a.id.cmp(&b.id))
    });

    Ok(Catalog { models, routing, model_enums, dynamic: true })
}

/// Fetches and merges `v1internal:fetchAvailableModels` across all candidate endpoints.
fn fetch_available_models_raw(token: &str, project_id: &str) -> Result<(serde_json::Map<String, Value>, Option<Value>), String> {
    let mut merged = serde_json::Map::new();
    let mut sorts: Vec<Value> = Vec::new();
    let mut any_ok = false;
    let mut last_err = String::new();
    let body = json!({"project": project_id});

    for endpoint in endpoint_candidates() {
        let resp = match with_headers(
            api::http()
                .post(format!("{endpoint}/v1internal:fetchAvailableModels"))
                .timeout(DISCOVERY_TIMEOUT)
                .json(&body),
            token,
        )
        .send()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = api::network_error(e);
                continue;
            }
        };
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            last_err = error_text(status, &text);
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(models_obj) = v["models"].as_object() else { continue };
        if models_obj.is_empty() {
            continue;
        }
        any_ok = true;
        for (k, val) in models_obj {
            if val.is_object() {
                merged.insert(k.clone(), val.clone());
            }
        }
        if let Some(arr) = v["agentModelSorts"].as_array() {
            sorts.extend(arr.iter().cloned());
        }
    }

    if !any_ok {
        return Err(if last_err.is_empty() { "Antigravity returned no model catalog".into() } else { last_err });
    }
    let merged_sorts = (!sorts.is_empty()).then_some(Value::Array(sorts));
    Ok((merged, merged_sorts))
}

/// Refreshes the in-memory and cached Antigravity model catalog.
pub(crate) fn refresh_catalog(token: &str, project_id: &str) -> Result<(), String> {
    let (raw, sorts) = fetch_available_models_raw(token, project_id)?;
    let cat = build_catalog(&raw, sorts.as_ref())?;
    save_cached(&cat);
    *CATALOG.lock().unwrap() = cat;
    Ok(())
}

/// Dynamic runtime model lookup for a single requested id (used on HTTP 404 or unknown custom model).
pub(crate) fn fetch_available_runtime_model(token: &str, project_id: &str, requested: &str, effort: Option<&str>) -> Option<String> {
    let (raw, _) = fetch_available_models_raw(token, project_id).ok()?;
    // Register any discovered enums.
    {
        let mut cat = CATALOG.lock().unwrap();
        for (wire_id, info) in &raw {
            if let Some(e) = info["model"].as_str().filter(|s| !s.is_empty()) {
                cat.model_enums.insert(wire_id.clone(), e.to_string());
            }
        }
    }
    let req = public_id(requested);
    // 1. Exact key
    if is_usable_runtime_model_id(req)
        && let Some(info) = raw.get(req)
        && is_selectable_runtime_model(req, info)
    {
        return Some(req.to_string());
    }
    // 2. Case-insensitive key
    let lower_req = req.to_ascii_lowercase();
    for (id, info) in &raw {
        if id.to_ascii_lowercase() == lower_req && is_selectable_runtime_model(id, info) {
            return Some(id.clone());
        }
    }
    // 3. Effort-ordered thinking-tier suffix resolution
    if is_usable_runtime_model_id(req) {
        let suffixes: &[&str] = match effort.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("xhigh" | "max" | "ultra") => &["-xhigh", "-high", "-medium", "-low", "-thinking", "-minimal", "-extra-low"],
            Some("high") => &["-high", "-xhigh", "-medium", "-low", "-thinking", "-minimal", "-extra-low"],
            Some("medium") => &["-medium", "-low", "-high", "-xhigh", "-thinking", "-minimal", "-extra-low"],
            _ => &["-low", "-minimal", "-extra-low", "-medium", "-high", "-xhigh", "-thinking"],
        };
        for suffix in suffixes {
            let want = format!("{lower_req}{suffix}");
            if let Some((id, _)) = raw.iter().find(|(k, v)| k.to_ascii_lowercase() == want && is_selectable_runtime_model(k, v)) {
                return Some(id.clone());
            }
        }
    }
    None
}

// ---------------------------------------------------------------- quota / usage

fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp_millis())
}

fn bucket_label(window: Option<&str>, name: &str, group_name: &str) -> &'static str {
    let w = window.unwrap_or("").to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    let g = group_name.to_ascii_lowercase();
    if w.contains("5h") || w.contains("hour") || n.contains("5h") || n.contains("5-hour") || n.contains("session") {
        "5h"
    } else if w.contains("week") || w.contains("7d") || n.contains("week") || n.contains("7d") {
        "Week"
    } else if w.contains("day") || w.contains("24h") || n.contains("daily") || n.contains("day") {
        "Day"
    } else if g.contains("claude") || n.contains("claude") {
        "Claude"
    } else if g.contains("gemini") || n.contains("gemini") {
        "Gemini"
    } else {
        "Quota"
    }
}

/// Parses `v1internal:retrieveUserQuotaSummary` into up to 2 Oak `Limit`s.
pub(crate) fn parse_quota_summary(data: &Value) -> Vec<Limit> {
    let mut out: Vec<Limit> = Vec::new();
    for group in data["groups"].as_array().into_iter().flatten() {
        let gname = group["displayName"].as_str().unwrap_or("");
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let Some(rem) = bucket["remainingFraction"].as_f64().filter(|v| v.is_finite()) else { continue };
            let used = ((1.0 - rem.clamp(0.0, 1.0)) * 100.0) as f32;
            let resets_at = bucket["resetTime"].as_str().and_then(parse_rfc3339_ms);
            let bname = bucket["displayName"].as_str().or_else(|| bucket["bucketId"].as_str()).unwrap_or("");
            let label = bucket_label(bucket["window"].as_str(), bname, gname);
            if let Some(existing) = out.iter_mut().find(|l| l.label == label) {
                if used > existing.percent {
                    existing.percent = used;
                    existing.resets_at = resets_at.or(existing.resets_at);
                }
            } else {
                out.push(Limit { label, percent: used, resets_at });
            }
        }
    }
    out.sort_by_key(|l| ["5h", "Day", "Week", "Gemini", "Claude", "Quota"].iter().position(|x| *x == l.label).unwrap_or(9));
    out.truncate(2);
    out
}

/// Derives quota limits from `fetchAvailableModels` per-model `quotaInfo`
/// (fallback when `retrieveUserQuotaSummary` is unavailable, e.g. free tier 403).
pub(crate) fn parse_model_quotas(raw_models: &serde_json::Map<String, Value>) -> Vec<Limit> {
    let mut gemini: Option<(f32, Option<i64>)> = None;
    let mut claude: Option<(f32, Option<i64>)> = None;
    let update = |slot: &mut Option<(f32, Option<i64>)>, used: f32, reset: Option<i64>| match slot {
        Some((p, r)) => {
            if used > *p {
                *p = used;
                *r = reset.or(*r);
            }
        }
        None => *slot = Some((used, reset)),
    };
    for (id, info) in raw_models {
        if !is_selectable_runtime_model(id, info) {
            continue;
        }
        let qi = &info["quotaInfo"];
        let Some(rem) = qi["remainingFraction"].as_f64().filter(|v| v.is_finite()) else { continue };
        let used = ((1.0 - rem.clamp(0.0, 1.0)) * 100.0) as f32;
        let reset = qi["resetTime"].as_str().and_then(parse_rfc3339_ms);
        let lower = id.to_ascii_lowercase();
        if lower.starts_with("claude-") {
            update(&mut claude, used, reset);
        } else if lower.starts_with("gemini-") {
            update(&mut gemini, used, reset);
        }
    }
    let mut out = Vec::new();
    if let Some((percent, resets_at)) = gemini {
        out.push(Limit { label: "Gemini", percent, resets_at });
    }
    if let Some((percent, resets_at)) = claude {
        out.push(Limit { label: "Claude", percent, resets_at });
    }
    out
}

fn fetch_quota_summary_best_effort(token: &str) -> Option<Value> {
    for endpoint in endpoint_candidates() {
        let Ok(resp) = with_headers(
            api::http()
                .post(format!("{endpoint}/v1internal:retrieveUserQuotaSummary"))
                .header("Accept", "application/json")
                .timeout(DISCOVERY_TIMEOUT)
                .json(&json!({})),
            token,
        )
        .send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Ok(v) = resp.json::<Value>() {
            return Some(v);
        }
    }
    None
}

/// Fetches account quota limits and (when `update_catalog` is true) updates the
/// dynamic model catalog from `fetchAvailableModels`.
pub(crate) fn fetch_usage_and_catalog(token: &str, project_id: &str, update_catalog: bool) -> Result<(Vec<Limit>, bool), String> {
    let summary = fetch_quota_summary_best_effort(token).map(|v| parse_quota_summary(&v)).unwrap_or_default();
    let models_res = fetch_available_models_raw(token, project_id);
    let mut catalog_ok = false;
    let mut fallback_limits = Vec::new();
    if let Ok((raw, sorts)) = models_res {
        fallback_limits = parse_model_quotas(&raw);
        if update_catalog
            && let Ok(cat) = build_catalog(&raw, sorts.as_ref())
        {
            save_cached(&cat);
            *CATALOG.lock().unwrap() = cat;
            catalog_ok = true;
        }
    } else if summary.is_empty() {
        return Err(models_res.unwrap_err());
    }
    let limits = if !summary.is_empty() { summary } else { fallback_limits };
    Ok((limits, catalog_ok))
}
