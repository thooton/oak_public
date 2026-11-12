//! One record of the usage log, and how two machines' records merge. The
//! log itself is Oak's; a plugin that shares it between machines reads
//! this machine's records and hands Oak the others' (`Host::usage_local`,
//! `Host::usage_set_shared`).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Older records are dropped when the log is read.
pub const KEEP_DAYS: i64 = 90;

/// The oldest time a record is kept from (`KEEP_DAYS` ago).
pub fn keep_from() -> i64 {
    crate::now_ms() - KEEP_DAYS * crate::time::DAY_MS
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}
fn is_zero_f(n: &f64) -> bool {
    *n == 0.0
}

/// One line of usage.jsonl.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Rec {
    /// Stable across retries and machines; assigned before a record is uploaded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// When (ms since 1970).
    pub t: i64,
    /// "req": a reply; "limits": an account's utilization, polled;
    /// "credits": OpenRouter credits left (`usd`).
    pub ev: String,
    /// The account: its email (OPENROUTER for OpenRouter).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub acct: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// "main", "summary" or "title".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub w5m: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub w1h: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub read: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub out: u64,
    /// Dollars: what an OpenRouter reply cost, or the credits left.
    #[serde(default, skip_serializing_if = "is_zero_f")]
    pub usd: f64,
    /// The 5-hour and weekly utilization (percent) and when each resets
    /// (ms since 1970): for a reply, as its headers had them, before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub u5: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r5: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub u7: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r7: Option<i64>,
    /// The conversation a reply was for (Claude), to follow its cache.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conv: String,
    /// Why the API says the reply missed the cache, when the request asked
    /// (`api::diagnostics`): "none" (it matched the previous request),
    /// "pending" (no answer in time), "absent" (no diagnostics came), or
    /// the API's `cache_miss_reason` type
    /// ("system_changed", "messages_changed", "unavailable", …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub miss: String,
    /// The API's estimate of the input tokens past the change it found.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub missed: u64,
    /// How the request went in a server-side thread: "new" or "cont"
    /// (only what it added was sent).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thr: String,
    /// Earlier thinking blocks the API dropped (not read, not billed: they
    /// came from another account or model, or no longer matched what came
    /// before them on an account that drops those), and those that no
    /// longer matched but were read and billed anyway.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub think_dropped: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub think_mismatched: u64,
}

/// Puts `r` in its place in `recs`, sorted as `merge_records` sorts (by
/// time, then id), unless it is there already (an id is only ever one
/// record, at one time). True when added.
pub fn insert_sorted(recs: &mut Vec<Rec>, r: &Rec) -> bool {
    match recs.binary_search_by(|x| x.t.cmp(&r.t).then_with(|| x.id.cmp(&r.id))) {
        Ok(_) => false,
        Err(i) => {
            recs.insert(i, r.clone());
            true
        }
    }
}

/// One list of `local` and `remote`, oldest first: what is recent enough
/// to keep, each id once.
pub fn merge_records(local: &[Rec], remote: &[Rec]) -> Vec<Rec> {
    let cut = keep_from();
    let mut seen = HashSet::new();
    let mut out: Vec<_> = local.iter().chain(remote).filter(|r| r.t >= cut && seen.insert(&r.id)).cloned().collect();
    out.sort_by(|a, b| a.t.cmp(&b.t).then_with(|| a.id.cmp(&b.id)));
    out
}
