//! Quick's recoverable stop reasons, and the fresh threads a conversation
//! moves to: on another account when one reaches its usage limit, on the same
//! one when a thread can't reach this device. A fresh thread is sent the whole
//! conversation (`agent::unseen_by`, `FRESH_KEY`), never a summary of it.

use super::*;

pub(crate) const DEVICE: &str = "quick_device_unavailable";
pub(crate) const LIMIT: &str = "quick_usage_limit";
pub(super) const LIMIT_NOTICE: &str =
    "You've reached your usage limit. Check your profile for details.";
pub(crate) const ERROR: &str = "quick_error";
pub(crate) const DEVICE_RETRY: &str = "[Oak] Please check for this device again with list_outposts. If the device is accessible, continue the existing task from where you left off, respecting its constraints. Do not repeat work already done.";
pub(crate) const ERROR_RETRY: &str = "[Oak] The previous Quick reply was interrupted by a service error. Please continue the existing task from where you left off. Check the results and current state before repeating any work.";
/// What a fresh thread is sent after the conversation so far when the last
/// account reached its usage limit (`agent::Worker::handoff_quick`).
pub(crate) const LIMIT_FRESH: &str = "[Oak] The previous AWS Quick account reached its usage limit, so this conversation goes on in this fresh thread, on another account; the conversation so far is above. Continue the existing task from where it left off, respecting its constraints. Do not repeat work already done.";
/// What a fresh thread is sent after the conversation so far when the last
/// thread couldn't reach this device, even when asked again (`fresh_session`).
pub(crate) const DEVICE_FRESH: &str = "[Oak] The previous AWS Quick thread couldn't reach this device, so this conversation goes on in this fresh thread; the conversation so far is above. Check for the device with list_outposts, and once it is accessible, continue the existing task from where it left off, respecting its constraints. Do not repeat work already done.";
/// Marks the message that opens a fresh thread (its value the thread's id):
/// Quick's replies before it are on another thread, so each request sends the
/// whole conversation until Quick replies on this one (`agent::unseen_by`).
pub(crate) const FRESH_KEY: &str = "_oak_quick_fresh";

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Session {
    pub(crate) account: String,
    pub(crate) thread: String,
}

pub(crate) fn note_limit(account: &str) {
    usage::note_limit(account, api::now_ms());
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
}
