//! Cloud sessions, as far as a plugin sees them: the accounts a backend
//! runs sessions on, a request to the sessions API, and the keys of their
//! tabs. The engine is Oak's (`plugin::CloudBackend` puts another service
//! behind it).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// An account sessions can run on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// Its id, as tab keys carry it. A plugin backend's start with the
    /// backend's id and "~" (`CloudBackend::owns`).
    pub rid: String,
    /// What the user knows it by (its email).
    pub label: String,
    /// Out of usage until then (ms since 1970), or 0.
    pub blocked_until: i64,
}

/// A request to Anthropic's sessions API on behalf of an account: the
/// backend adds the login and sends it (`CloudBackend::request`).
#[derive(Clone, Debug)]
pub struct Call {
    /// "GET", "POST", ...
    pub method: String,
    /// The path and query, from the API's root ("/v1/code/sessions").
    pub path: String,
    /// The `anthropic-beta` header, when the call needs one.
    pub beta: Option<String>,
    /// The organization the call is scoped to.
    pub org: Option<String>,
    pub body: Option<Value>,
    /// Bounds the whole request.
    pub timeout: Duration,
}

impl Call {
    /// The call as the interface carries it (`CloudBackend`'s "request").
    pub fn to_json(&self, rid: &str) -> Value {
        json!({"rid": rid, "method": self.method, "path": self.path, "beta": self.beta, "org": self.org, "body": self.body, "timeout_ms": self.timeout.as_millis() as u64})
    }

    pub fn from_json(v: &Value) -> Call {
        let text = |name: &str| v[name].as_str().map(str::to_string);
        Call {
            method: text("method").unwrap_or_default(),
            path: text("path").unwrap_or_default(),
            beta: text("beta"),
            org: text("org"),
            body: Some(v["body"].clone()).filter(|b| !b.is_null()),
            timeout: Duration::from_millis(v["timeout_ms"].as_u64().unwrap_or(30_000)),
        }
    }

    /// The headers every sessions request carries after its login: what it
    /// accepts, the API version, the beta and organization when given, as
    /// (name, value).
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![("Accept".to_string(), if self.path.contains("/events/stream") { "text/event-stream" } else { "application/json" }.to_string())];
        if let Some(b) = &self.beta {
            h.push(("anthropic-beta".into(), b.clone()));
        }
        if let Some(o) = &self.org {
            h.push(("x-organization-uuid".into(), o.clone()));
        }
        h
    }
}

/// Tabs of cloud sessions are named `cloud:<account>:<session>`.
pub const KEY_PREFIX: &str = "cloud:";

pub fn is_key(key: &str) -> bool {
    key.starts_with(KEY_PREFIX)
}

/// The key of session `id` on account `rid`.
pub fn key(rid: &str, id: &str) -> String {
    format!("{KEY_PREFIX}{rid}:{id}")
}

/// The account and session a key names.
pub fn parse_key(key: &str) -> Option<(String, String)> {
    let (rid, id) = key.strip_prefix(KEY_PREFIX)?.split_once(':')?;
    Some((rid.to_string(), id.to_string()))
}

/// A file a backend's server keeps for its sessions.
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    pub name: String,
    pub size: u64,
    pub modified: i64,
}
