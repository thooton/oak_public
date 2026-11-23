// Antigravity: Google's Cloud Code Assist / Antigravity models (Gemini, plus
// the Claude and GPT-OSS models it serves), through a Google account sign-in.
// Signing in is OAuth 2.0 + PKCE (a browser and a callback on
// localhost:51121/oauth-callback, with manual URL paste fallback); logins are
// kept in antigravity.json. Requests go to Cloud Code Assist's
// v1internal:streamGenerateContent?alt=sse endpoint, following pi-antigravity
// (github.com/Rahularya01/pi-antigravity).
//
// Model ids in Oak are prefixed with "antigravity/" (for example
// "antigravity/gemini-3.8-flash" and "antigravity/claude-opus-4-6") so they
// never collide with Claude's own model ids. The catalog is bootstrapped from
// static defaults and refreshed dynamically from v1internal:fetchAvailableModels.
//
// Thought signatures returned by Gemini 3+ models are stored on Oak's
// reasoning, text and tool_call blocks (`sig`, `by: "antigravity"`,
// `sig_model`) and replayed when continuing on the same model; a call another
// provider's or model's made (after a switch) stays a real function call,
// with the stand-in signature Google documents for it, so Gemini 3 accepts
// the history and the model reads it as its own.

mod auth;
mod catalog;
mod wire;

pub(crate) use auth::room;
pub use auth::{
    AccountView, Login, account, account_for_model, accounts, accounts_detailed, finish_login,
    forget_remote, load, login_result, movable, move_to_vault, note, on, refresh, remote, sign_out,
    start_login, sync,
};
#[cfg(test)]
pub use catalog::QuotaPool;
pub use catalog::{context_window, efforts, max_output, model_name, models, serves};
#[cfg(test)]
pub(crate) use wire::request_body;
pub use wire::{preamble, stream};

use crate::api;
use sha2::{Digest, Sha256};

/// Oak model id prefix for Antigravity models.
pub const PREFIX: &str = "antigravity/";

/// Primary and fallback Cloud Code Assist endpoints.
pub(crate) const DEFAULT_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub(crate) const ENDPOINT_FALLBACKS: [&str; 2] =
    [DEFAULT_ENDPOINT, "https://cloudcode-pa.googleapis.com"];

/// User-Agent matching the official Antigravity CLI wire fingerprint so newer
/// catalog entries (such as Gemini 4 Argon/Neon) are advertised.
pub(crate) const DEFAULT_USER_AGENT: &str = "antigravity/cli/1.5.5 (aidev_client; os_type=linux; arch=amd64; cl=982146307; auth_method=consumer)";

/// Reads `ANTIGRAVITY_<name>` (or legacy `NOAGY_<name>`), trimming whitespace.
pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var(format!("ANTIGRAVITY_{name}"))
        .or_else(|_| std::env::var(format!("NOAGY_{name}")))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Candidate API base URLs: `OAK_ANTIGRAVITY_URL` or `ANTIGRAVITY_BASE_URL`
/// when overridden (for testing), else the daily and production endpoints.
pub(crate) fn endpoint_candidates() -> Vec<String> {
    if let Ok(url) = std::env::var("OAK_ANTIGRAVITY_URL")
        && !url.trim().is_empty()
    {
        return vec![url.trim().trim_end_matches('/').to_string()];
    }
    if let Some(url) = env_var("BASE_URL") {
        return vec![url.trim_end_matches('/').to_string()];
    }
    ENDPOINT_FALLBACKS.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn user_agent() -> String {
    env_var("USER_AGENT").unwrap_or_else(|| DEFAULT_USER_AGENT.to_string())
}

/// Deterministic RFC 4122 UUID-shaped identifier derived from `seed`.
pub(crate) fn stable_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    api::uuid_text(&bytes)
}

/// Deterministic fallback project id when Cloud Code Assist discovery fails.
pub(crate) fn default_project_id(seed: &str) -> String {
    env_var("PROJECT_ID").unwrap_or_else(|| stable_uuid(&format!("antigravity:{seed}")))
}

/// Applies the standard Antigravity API headers to a request builder.
pub(crate) fn with_headers(
    req: reqwest::blocking::RequestBuilder,
    token: &str,
) -> reqwest::blocking::RequestBuilder {
    req.bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
}

/// Formats an HTTP error reply concisely from a Google/JSON error payload.
pub(crate) fn error_text(status: u16, body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let msg = crate::provider::error_msg(&v, body, None);
    let lower = msg.to_ascii_lowercase();
    if lower.contains("exceeds the maximum number of tokens")
        || lower.contains("token count exceeds")
        || lower.contains("context length")
        || lower.contains("prompt is too long")
        || lower.contains("request payload size exceeds")
    {
        format!("HTTP {status}: prompt is too long: {msg}")
    } else {
        format!("HTTP {status}: {msg}")
    }
}

#[cfg(test)]
mod tests;
