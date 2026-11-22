//! Quick's usage limits, read as its desktop app reads them: a POST of `{}`
//! to the workspace's `/integration/quick-work/user-info`, whose
//! `usageSummary` holds the month's units (`monthlyUsage`: `availableUnits`
//! of `provisionedUnits`, `usedPercentage`, and `resetsAt` in seconds), the
//! session window (`sessionUsage`: `usedPercentage`, `resumeInMinutes`
//! while it is used up), the subscription (`QUICK_LITE_FREE_TRIAL`, its
//! trial's `expiryDate`) and whether the account may run at all
//! (`entitlementStatus`).
//!
//! It is fetched with the other providers' usage (`api::refresh_accounts`,
//! every two minutes and after a sign-in) and kept here, not in quick.json:
//! it is a reading, not part of the login. A failed fetch keeps the last
//! reading, which the box marks as of then.

use super::*;
use std::sync::Mutex;

/// What `user-info` last said about the account's usage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// Percent of the month's units used, as Quick rounds it.
    pub month_percent: f32,
    /// Units left of the month's, and how many it has.
    pub units_left: Option<u64>,
    pub units_total: Option<u64>,
    /// When the month's units come back (ms).
    pub month_resets_at: Option<i64>,
    /// Percent of the session window used, and when it lets requests in
    /// again (ms) while it is used up.
    pub session_percent: f32,
    pub session_resumes_at: Option<i64>,
    /// The subscription, as Quick names it ("QUICK_LITE_FREE_TRIAL").
    pub plan: String,
    /// When its free trial ends (ms).
    pub trial_ends_at: Option<i64>,
    /// `entitlementStatus`: "ALLOWED" while the account may run.
    pub entitlement: String,
}

impl Usage {
    /// Quick lets the account run.
    pub fn allowed(&self) -> bool {
        self.entitlement.is_empty() || self.entitlement.eq_ignore_ascii_case("ALLOWED")
    }

    /// The month's units used, when Quick says how many it has.
    pub fn units_used(&self) -> Option<u64> {
        Some(self.units_total?.saturating_sub(self.units_left?))
    }
}

/// The last reading, the error of the last fetch (cleared by a good one),
/// and when the reading was taken (ms).
#[derive(Default)]
struct Kept {
    usage: Option<Usage>,
    error: Option<String>,
    at: i64,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { usage: None, error: None, at: 0 });

/// The last reading, the last fetch's error, and when the reading was taken.
pub fn last() -> (Option<Usage>, Option<String>, i64) {
    let k = KEPT.lock().unwrap();
    (k.usage.clone(), k.error.clone(), k.at)
}

/// A time Quick sends in seconds (a value past 1e12 is taken as ms already).
fn ms(v: &Value) -> Option<i64> {
    let n = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).filter(|&n| n > 0)?;
    Some(if n > 1_000_000_000_000 { n } else { n * 1000 })
}

/// The usage in `user-info`'s answer `v`, at `now` (ms): None when it has no
/// month's reading.
pub fn parse(v: &Value, now: i64) -> Option<Usage> {
    let s = &v["usageSummary"];
    let m = &s["monthlyUsage"];
    if !m.is_object() {
        return None;
    }
    let units_left = m["availableUnits"].as_u64();
    let units_total = m["provisionedUnits"].as_u64();
    let month_percent = match m["usedPercentage"].as_f64() {
        Some(p) => p as f32,
        // Worked out from the units when Quick leaves the percent out.
        None => match (units_left, units_total) {
            (Some(l), Some(t)) if t > 0 => (t.saturating_sub(l) as f32 / t as f32) * 100.0,
            _ => return None,
        },
    };
    let session = &s["sessionUsage"];
    let resume = session["resumeInMinutes"].as_f64().filter(|&m| m > 0.0);
    Some(Usage {
        month_percent: month_percent.clamp(0.0, 100.0),
        units_left,
        units_total,
        month_resets_at: ms(&m["resetsAt"]),
        session_percent: session["usedPercentage"].as_f64().unwrap_or(0.0).clamp(0.0, 100.0) as f32,
        session_resumes_at: resume.map(|m| now + (m * 60_000.0) as i64),
        plan: s["subscription"]["subscriptionType"].as_str().unwrap_or("").to_string(),
        trial_ends_at: ms(&s["subscription"]["freeTrialMetadata"]["expiryDate"]),
        entitlement: s["entitlementStatus"].as_str().unwrap_or("").to_string(),
    })
}

/// "Lite trial" for "QUICK_LITE_FREE_TRIAL", "Pro" for "QUICK_PRO": the
/// subscription as the box names it, None when Quick names none.
pub fn plan_label(plan: &str) -> Option<String> {
    let words: Vec<&str> = plan.split('_').filter(|w| !w.is_empty() && !w.eq_ignore_ascii_case("quick")).collect();
    let (words, trial) = match words.as_slice() {
        [rest @ .., f, t] if f.eq_ignore_ascii_case("free") && t.eq_ignore_ascii_case("trial") => (rest.to_vec(), true),
        [rest @ .., t] if t.eq_ignore_ascii_case("trial") => (rest.to_vec(), true),
        _ => (words, false),
    };
    let title = |w: &str| {
        let lower = w.to_ascii_lowercase();
        let mut c = lower.chars();
        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
    };
    let name = words.iter().map(|w| title(w)).collect::<Vec<_>>().join(" ");
    match (name.is_empty(), trial) {
        (true, false) => None,
        (true, true) => Some("trial".into()),
        (false, true) => Some(format!("{name} trial")),
        (false, false) => Some(name),
    }
}

/// Asks Quick for the account's usage.
fn fetch() -> Result<Usage, String> {
    let access = login::access_token()?;
    let host = login::host()?;
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/user-info"))
        .bearer_auth(access)
        .header("Content-Type", "application/json")
        .body("{}")
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(api::network_error)?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        let why = serde_json::from_str::<Value>(&text).map(|v| provider::error_msg(&v, &text, None)).unwrap_or_else(|_| text.chars().take(300).collect());
        return Err(format!("Quick's usage ({status}): {why}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("Quick's usage was unreadable: {e}"))?;
    parse(&v, api::now_ms()).ok_or_else(|| "Quick's answer had no monthly usage".to_string())
}

/// Fetches the usage again (in the background, with the other providers').
/// Signed out, the reading is forgotten.
pub fn refresh() {
    if !login::on() {
        *KEPT.lock().unwrap() = Kept::default();
        return;
    }
    let r = fetch();
    let mut k = KEPT.lock().unwrap();
    match r {
        Ok(u) => {
            k.usage = Some(u);
            k.error = None;
            k.at = api::now_ms();
        }
        Err(e) => k.error = Some(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `user-info`'s answer as Quick's desktop app got it (startup.saz),
    /// without who it was.
    const CAPTURED: &str = r#"{"role":"EXPLORER","usageSummary":{"accountOwnership":"OWNER","entitlementStatus":"ALLOWED","monthlyUsage":{"availableUnits":582,"provisionedUnits":600,"resetsAt":1796083200,"usedPercentage":3},"overageCapMinutes":null,"overageConsumedMinutes":null,"overageEnabled":false,"sessionUsage":{"resumeInMinutes":0,"usedPercentage":0},"subscription":{"freeTrialMetadata":{"expiryDate":1797938670,"status":"ACTIVE"},"subscriptionType":"QUICK_LITE_FREE_TRIAL"}}}"#;

    #[test]
    fn the_captured_answer_reads_as_its_month() {
        let now = 1_795_000_000_000;
        let u = parse(&serde_json::from_str(CAPTURED).unwrap(), now).unwrap();
        assert_eq!(u.month_percent, 3.0);
        assert_eq!((u.units_left, u.units_total, u.units_used()), (Some(582), Some(600), Some(18)));
        // December 1, 2026, 00:00 UTC.
        assert_eq!(u.month_resets_at, Some(1_796_083_200_000));
        assert_eq!((u.session_percent, u.session_resumes_at), (0.0, None));
        assert_eq!(u.trial_ends_at, Some(1_797_938_670_000));
        assert!(u.allowed());
        assert_eq!(plan_label(&u.plan).as_deref(), Some("Lite trial"));
    }

    #[test]
    fn a_used_up_session_says_when_it_resumes_and_the_percent_can_come_from_units() {
        let now = 1_000_000;
        let v = serde_json::json!({"usageSummary": {"entitlementStatus": "DENIED", "monthlyUsage": {"availableUnits": 150, "provisionedUnits": 600}, "sessionUsage": {"resumeInMinutes": 12, "usedPercentage": 100}}});
        let u = parse(&v, now).unwrap();
        assert_eq!(u.month_percent, 75.0);
        assert_eq!((u.session_percent, u.session_resumes_at), (100.0, Some(now + 12 * 60_000)));
        assert!(!u.allowed());
        assert_eq!(parse(&serde_json::json!({"email": "x"}), now), None);
    }

    #[test]
    fn plans_are_named_as_the_box_shows_them() {
        assert_eq!(plan_label("QUICK_PRO").as_deref(), Some("Pro"));
        assert_eq!(plan_label("QUICK_ENTERPRISE_PLUS").as_deref(), Some("Enterprise Plus"));
        assert_eq!(plan_label("FREE_TRIAL").as_deref(), Some("trial"));
        assert_eq!(plan_label(""), None);
    }
}
