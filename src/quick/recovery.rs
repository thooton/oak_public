//! Quick's recoverable stop reasons, and the fresh threads a conversation
//! moves to: on another account when one reaches its usage limit, on the same
//! one when a thread can't reach this device. A fresh thread is sent the whole
//! conversation (`agent::unseen_by`, `FRESH_KEY`), never a summary of it.

use super::*;

pub(crate) const DEVICE: &str = "quick_device_unavailable";
pub(crate) const LIMIT: &str = "quick_usage_limit";
pub(super) const LIMIT_NOTICE: &str =
    "You've reached your usage limit. Check your profile for details.";
/// Quick's service turned the account's requests away for a while (a rate
/// limit, throttling): the conversation goes on on another account, as at a
/// usage limit, or waits and retries on this one when there is none.
pub(crate) const RATE: &str = "quick_rate_limit";
pub(crate) const ERROR: &str = "quick_error";
pub(crate) const DEVICE_RETRY: &str = "[Oak] Please check for this device again with list_outposts. If the device is accessible, continue the existing task from where you left off, respecting its constraints. Do not repeat work already done.";
pub(crate) const ERROR_RETRY: &str = "[Oak] The previous Quick reply was interrupted by a service error. Please continue the existing task from where you left off. Check the results and current state before repeating any work.";
/// What a fresh thread is sent after the conversation so far when the last
/// account reached its usage limit (`agent::Worker::handoff_quick`).
pub(crate) const LIMIT_FRESH: &str = "[Oak] The previous AWS Quick account reached its usage limit, so this conversation goes on in this fresh thread, on another account; the conversation so far is above. Continue the existing task from where it left off, respecting its constraints. Do not repeat work already done.";
/// What a fresh thread is sent after the conversation so far when the last
/// account was rate-limited (`agent::Worker::handoff_quick`).
pub(crate) const RATE_FRESH: &str = "[Oak] The previous AWS Quick account was rate-limited, so this conversation goes on in this fresh thread, on another account; the conversation so far is above. Continue the existing task from where it left off, respecting its constraints. Do not repeat work already done.";
/// What a fresh thread is sent after the conversation so far when the last
/// thread couldn't reach this device, even when asked again (`fresh_session`).
pub(crate) const DEVICE_FRESH: &str = "[Oak] The previous AWS Quick thread couldn't reach this device, so this conversation goes on in this fresh thread; the conversation so far is above. Check for the device with list_outposts, and once it is accessible, continue the existing task from where it left off, respecting its constraints. Do not repeat work already done.";
/// Marks the message that opens a fresh thread (its value the thread's id):
/// Quick's replies before it are on another thread, so each request sends the
/// whole conversation until Quick replies on this one (`agent::unseen_by`).
pub(crate) const FRESH_KEY: &str = "_oak_quick_fresh";

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Session {
    pub(crate) account: String,
    pub(crate) thread: String,
    /// A thread a revert opened (`agent::quick_thread_reverted`) holds none
    /// of the conversation's first `from` messages: Quick's replies among them
    /// are another thread's, and so are the messages it took in mid-turn. (A
    /// thread opened by a message says so on it instead, `FRESH_KEY`.) Kept
    /// as a number, so a revert changes no message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<usize>,
    /// A summary opened this thread (`compacted_session`): none of the
    /// conversation is in it, and its `from` is set by the first request
    /// that goes to it (`agent::open_compacted_thread`), so it is sent the
    /// summary and all after it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) compacted: bool,
}

/// The thread conversation `conv` goes on in once a summary replaces the
/// older part of it: a fresh one on the same account (`current`'s, else the
/// seat's), which Quick is sent the summary and the part kept (as a revert's
/// is). None when Quick never had a thread there.
pub(crate) fn compacted_session(conv: &str, current: Option<&Session>) -> Option<Session> {
    let account = current
        .map(|q| q.account.clone())
        .filter(|a| !a.is_empty())
        .or_else(|| crate::rotation::seat(api::Provider::Quick, conv))?;
    Some(Session { account, thread: hex_uuid(), from: None, compacted: true })
}

/// The thread a summary of conversation `conv` is written on: a throwaway
/// one on the account its thread is on, so the summary is Quick's own and
/// the conversation's thread is left as it is. None with no account.
pub(crate) fn summary_session(conv: &str, current: Option<&Session>) -> Option<Session> {
    let account = current
        .map(|q| q.account.clone())
        .filter(|a| !a.is_empty())
        .or_else(|| crate::rotation::seat(api::Provider::Quick, conv))
        .or_else(|| login::all().first().map(|l| l.id.clone()))?;
    Some(Session { account, thread: hex_uuid(), from: None, compacted: false })
}

pub(crate) fn note_limit(account: &str) {
    usage::note_limit(account, api::now_ms());
}

/// `account` was rate-limited: rotation passes it over for a few minutes.
pub(crate) fn note_rate_limit(account: &str) {
    usage::note_rate_limit(account, api::now_ms());
}

pub(crate) fn next_session(
    conv: &str,
    excluded: &std::collections::HashSet<String>,
) -> Option<Session> {
    let now = api::now_ms();
    let mut accounts = candidates(now);
    accounts.retain(|a| !excluded.contains(&a.id));
    let account = crate::rotation::preview(api::Provider::Quick, conv, &accounts, now)?;
    Some(Session {
        account,
        thread: hex_uuid(),
        from: None,
        compacted: false,
    })
}

/// A fresh thread on the conversation's own account (its seat), for when its
/// thread can't reach this device: a cold start's first thread often never
/// gets it. None while the conversation has no seat.
pub(crate) fn fresh_session(conv: &str) -> Option<Session> {
    let account = crate::rotation::seat(api::Provider::Quick, conv)?;
    Some(Session {
        account,
        thread: hex_uuid(),
        from: None,
        compacted: false,
    })
}

/// Match the service's notice, not replies that merely discuss a usage limit.
pub(crate) fn limit_notice(text: &str) -> bool {
    let text = text.trim().replace('’', "'").to_ascii_lowercase();
    matches!(
        text.as_str(),
        "you've reached your usage limit. check your profile for details."
            | "you've reached your usage limit."
            | "you have reached your usage limit. check your profile for details."
            | "you have reached your usage limit."
    )
}

pub(super) fn limit_error(p: &Value) -> bool {
    ["code", "errorCode", "error_code", "type"]
        .into_iter()
        .any(|key| {
            p[key]
                .as_str()
                .is_some_and(|code| code.eq_ignore_ascii_case("usage_limit_reached"))
        })
        || ["message", "error"]
            .into_iter()
            .any(|key| p[key].as_str().is_some_and(limit_notice))
        || p["error"].is_object() && limit_error(&p["error"])
}

/// An error frame's payload that says the account is rate-limited or
/// throttled (by its code or its message), not any other error: only these
/// and the usage limit move a conversation to another account.
pub(super) fn rate_error(p: &Value) -> bool {
    ["code", "errorCode", "error_code", "type", "status", "statusCode"]
        .into_iter()
        .any(|key| match &p[key] {
            Value::String(code) => rate_code(code),
            Value::Number(n) => n.as_u64() == Some(429),
            _ => false,
        })
        || ["message", "error"]
            .into_iter()
            .any(|key| p[key].as_str().is_some_and(rate_limited))
        || p["error"].is_object() && rate_error(&p["error"])
}

fn rate_code(code: &str) -> bool {
    let c = code.to_ascii_lowercase().replace(['-', ' '], "_");
    c.contains("throttl")
        || c.contains("rate_limit")
        || c.contains("ratelimit")
        || c.contains("too_many_requests")
        || c.contains("toomanyrequests")
        || c == "rate_exceeded"
        || c == "429"
}

/// Whether an error Quick's service or its socket gave (not a reply's text)
/// says the account is rate-limited: "Too many requests", "Rate exceeded",
/// throttling, a rate limit or an HTTP 429.
pub(crate) fn rate_limited(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    let http_429 = e.match_indices("429").any(|(i, _)| {
        let digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
        !digit(e[..i].chars().next_back()) && !digit(e[i + 3..].chars().next())
    });
    http_429
        || e.contains("too many requests")
        || e.contains("rate exceeded")
        || e.contains("rate limit")
        || e.contains("rate-limit")
        || e.contains("ratelimit")
        || e.contains("throttl")
}

/// list_outposts can return the object, JSON text, or a tool content envelope.
fn outposts(v: &Value, depth: u8) -> Option<bool> {
    if depth == 0 {
        return None;
    }
    match v {
        Value::Object(_) => {
            if let Some(devices) = v["outposts"].as_array() {
                if !devices.is_empty() {
                    return Some(true);
                }
                if v["scope"] == "current_turn_access"
                    || v["note"].as_str().is_some_and(no_device_note)
                {
                    return Some(false);
                }
            }
            ["text", "content", "result", "output", "note"]
                .into_iter()
                .find_map(|k| outposts(&v[k], depth - 1))
        }
        Value::Array(a) => a.iter().find_map(|x| outposts(x, depth - 1)),
        Value::String(s) => outpost_status(s, depth - 1),
        _ => None,
    }
}

fn no_device_note(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("this turn did not originate from a user device")
        && text.contains("no user devices are accessible from this turn")
}

fn outpost_status(text: &str, depth: u8) -> Option<bool> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| outposts(&v, depth))
        .or_else(|| no_device_note(text).then_some(false))
}

pub(super) fn outpost_missing(text: &str) -> bool {
    outpost_status(text, 5) == Some(false)
}
pub(super) fn outpost_available(text: &str) -> bool {
    outpost_status(text, 5) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_DEVICE: &str = "This turn did not originate from a user device, so no user devices are accessible from this turn.";

    #[test]
    fn recognizes_the_scoped_device_refusal_in_tool_results() {
        assert!(outpost_missing(
            &json!({"outposts": [], "scope": "current_turn_access", "note": NO_DEVICE}).to_string()
        ));
        assert!(outpost_missing(&json!({"content": [{"type": "text", "text": json!({"outposts": [], "note": NO_DEVICE}).to_string()}]}).to_string()));
        assert!(!outpost_missing(
            "{\"outposts\":[],\"scope\":\"all_devices\"}"
        ));
        assert!(outpost_available("{\"outposts\":[{\"id\":\"oak\"}]}"));
    }

    #[test]
    fn recognizes_a_quota_notice_but_not_discussion_of_limits() {
        assert!(limit_notice(LIMIT_NOTICE));
        assert!(limit_error(
            &json!({"error": {"code": "USAGE_LIMIT_REACHED"}})
        ));
        assert!(!limit_notice(
            "A user can reach their usage limit. Check your profile for details."
        ));
        assert!(!limit_error(
            &json!({"code": "RATE_LIMITED", "message": "Too many requests"})
        ));
    }

    /// A rate limit is its own reason (`RATE`), and only a rate limit is:
    /// other service errors don't move the conversation to another account.
    #[test]
    fn recognizes_a_rate_limit_but_not_other_errors() {
        for p in [
            json!({"code": "RATE_LIMITED", "message": "Too many requests"}),
            json!({"error": {"code": "ThrottlingException"}}),
            json!({"errorCode": "TooManyRequestsException"}),
            json!({"statusCode": 429}),
            json!({"message": "Rate exceeded"}),
        ] {
            assert!(rate_error(&p), "{p}");
            assert!(!limit_error(&p), "{p}");
        }
        for p in [
            json!({"code": "INTERNAL_ERROR", "message": "Something went wrong"}),
            json!({"code": "USAGE_LIMIT_REACHED"}),
            json!({"message": "Session 14290 not found"}),
            json!({"error": "Validation failed"}),
        ] {
            assert!(!rate_error(&p), "{p}");
        }
        assert!(rate_limited("Quick socket: HTTP 429 Too Many Requests"));
        assert!(rate_limited("Quick: request throttled"));
        assert!(!rate_limited("Quick: connection reset (os error 10054)"));
        assert!(!rate_limited("AWS Quick stream ended unexpectedly"));
    }
}
