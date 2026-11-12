//! Usage-limit resets Anthropic gives a Claude account ("Full reset",
//! "Fable reset", "5-hour reset", "Weekly reset"), each to use once before
//! it expires: the ones Claude Code's /limit-reset lists and uses.
//!
//! The usage report lists them when asked (`USAGE_PATH`: its `cedar_ember`
//! block, the program's name), and a POST to the organization's
//! reset_rate_limits uses one. Using one changes the account's limits for
//! good, so only the person does it, in Home's confirmation dialog
//! (`crate::resets_view`), which reads the account's resets afresh and
//! says what the reset does first: nothing else here calls `use_reset`,
//! and no agent, rotation or retry may (`crate::rotation` moves a
//! conversation to another account at a limit; it never uses a reset).

use super::*;

/// The usage report, with the account's resets (and without the spend,
/// which OAK doesn't show and Claude Code skips too).
pub(super) const USAGE_PATH: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";

/// The reset program the usage report and reset_rate_limits name.
const PROGRAM: &str = "cedar_ember";

/// How long using a reset may take before it is given up on (as Claude
/// Code does): it may still have gone through.
const USE_TIMEOUT: Duration = Duration::from_secs(35);

/// What a reset refills, as Claude Code names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The 5-hour limit.
    Session,
    /// The weekly limits.
    Weekly,
    /// Gives back to the weekly limit what Fable used ("Fable reset").
    Fable,
    /// The 5-hour and weekly limits.
    Full,
}

impl Kind {
    /// "Full reset".
    pub fn name(self) -> &'static str {
        match self {
            Kind::Session => "5-hour reset",
            Kind::Weekly => "Weekly reset",
            Kind::Fable => "Fable reset",
            Kind::Full => "Full reset",
        }
    }

    /// "full reset", inside a sentence.
    pub fn lower(self) -> String {
        match self {
            Kind::Weekly => "weekly reset".into(),
            Kind::Full => "full reset".into(),
            k => k.name().into(),
        }
    }
}

/// One reset on offer to an account (a "grant"): it may hold several uses.
#[derive(Clone, Debug, PartialEq)]
pub struct Reset {
    /// What reset_rate_limits is told to use.
    pub id: String,
    pub kind: Kind,
    /// The account's own name for it, when the report gives one.
    pub label: String,
    /// Uses left, of `total`.
    pub left: u32,
    pub total: u32,
    /// When it can first be used, and when it expires (ms), when said.
    pub starts_at: Option<i64>,
    pub expires_at: Option<i64>,
    /// The limits it refills (the report's keys: "five_hour", "seven_day", …).
    pub clears: Vec<String>,
    /// It gives back to the weekly limit what Fable used, instead.
    pub gives_back_weekly: bool,
    pub paused: bool,
    /// Anthropic would take it now.
    pub usable_now: bool,
    /// It only works at a limit it refills (false: usable any time).
    pub needs_limit: bool,
    /// How much of each limit it refills is used (percent).
    pub percent_used: Vec<(String, f64)>,
    /// The limits the account is at that it doesn't refill.
    pub blocking: Vec<String>,
    /// Why it can't be used now, in the report's words ("cooldown", …).
    pub unavailable: Option<String>,
}

/// The resets an account has, as its usage report last said.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resets {
    /// Whether the account takes part (false: `ineligible` says why).
    pub eligible: bool,
    pub ineligible: Option<String>,
    /// It is at a limit now, and which ones.
    pub at_limit: bool,
    pub exhausted: Vec<String>,
    pub grants: Vec<Reset>,
    /// When the weekly limits reset (ms): using a reset doesn't move it.
    pub weekly_resets_at: Option<i64>,
    /// Another reset was just used: none can be until then (ms).
    pub cooldown_until: Option<i64>,
}

impl Resets {
    /// The resets the account holds now: not expired, with a use left.
    pub fn held(&self, now: i64) -> Vec<&Reset> {
        self.grants.iter().filter(|g| g.held(now)).collect()
    }

    /// The reset `id`, when the account holds it.
    pub fn get(&self, id: &str, now: i64) -> Option<&Reset> {
        self.grants.iter().find(|g| g.id == id && g.held(now))
    }
}

/// The resets in usage report `u`: None when it lists none (the account
/// isn't in the program, or the report didn't say).
pub(super) fn read_resets(u: &Value) -> Option<Resets> {
    let c = u.get(PROGRAM).filter(|c| c.is_object())?;
    let keys = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect()).unwrap_or_default() };
    let time = |v: &Value| v.as_str().and_then(crate::time::parse_time);
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let grants = c["grants"].as_array().map(Vec::as_slice).unwrap_or_default();
    let grants = grants
        .iter()
        .filter_map(|g| {
            let id = g["id"].as_str().filter(|id| valid_id(id))?.to_string();
            let clears = keys(&g["clears"]);
            let gives_back_weekly = g["gives_back_weekly"].as_bool() == Some(true);
            let kind = if gives_back_weekly {
                Kind::Fable
            } else {
                match g["type"].as_str() {
                    Some("session") => Kind::Session,
                    Some("weekly") => Kind::Weekly,
                    Some("full") => Kind::Full,
                    _ if clears.len() == 1 && clears[0] == "five_hour" => Kind::Session,
                    _ if !clears.is_empty() && !clears.iter().any(|k| k == "five_hour") => Kind::Weekly,
                    _ => Kind::Full,
                }
            };
            let count = |v: &Value| v.as_u64().map_or(0, |n| n.min(u64::from(u32::MAX)) as u32);
            let mut percent_used: Vec<(String, f64)> = g["percent_used"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect()).unwrap_or_default();
            percent_used.sort_by(|a, b| a.0.cmp(&b.0));
            Some(Reset {
                id,
                kind,
                label: text(&g["label"]).unwrap_or_default(),
                left: count(&g["resets_left"]),
                total: count(&g["resets_total"]),
                starts_at: time(&g["starts_at"]),
                expires_at: time(&g["ends_at"]),
                clears,
                gives_back_weekly,
                paused: g["paused"].as_bool() == Some(true),
                usable_now: g["usable_now"].as_bool() == Some(true),
                needs_limit: g["use_requires_limit"].as_bool() != Some(false),
                percent_used,
                blocking: keys(&g["blocking"]),
                unavailable: text(&g["unavailable_reason"]),
            })
        })
        .collect();
    Some(Resets {
        eligible: c["eligible"].as_bool() == Some(true),
        ineligible: text(&c["ineligible_reason"]),
        at_limit: c["at_limit"].as_bool() == Some(true),
        exhausted: keys(&c["exhausted"]),
        grants,
        weekly_resets_at: time(&c["weekly_resets_at"]),
        cooldown_until: time(&c["cooldown_until"]),
    })
}

/// A reset id as reset_rate_limits takes one.
fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// What a limit key is called: "weekly limit".
pub fn limit_name(key: &str) -> String {
    match key {
        "five_hour" => "5-hour limit".into(),
        "seven_day" => "weekly limit".into(),
        "seven_day_opus" => "Opus weekly limit".into(),
        "seven_day_sonnet" => "Sonnet weekly limit".into(),
        "seven_day_overage_included" => "included-usage weekly limit".into(),
        "seven_day_oauth_apps" => "apps weekly limit".into(),
        "seven_day_cowork" => "Cowork weekly limit".into(),
        k => format!("{} limit", k.replace('_', " ")),
    }
}

/// "refills your 5-hour and weekly limits", for limit keys `keys`.
fn limits_phrase(keys: &[String], verb: &str) -> String {
    let names: Vec<String> = keys.iter().map(|k| limit_name(k).trim_end_matches(" limit").to_string()).collect();
    match names.as_slice() {
        [] => format!("{verb} your limits"),
        [one] => format!("{verb} your {one} limit"),
        [rest @ .., last] => format!("{verb} your {} and {last} limits", rest.join(", ")),
    }
}

impl Reset {
    /// Not expired, with a use left (or usable now all the same).
    pub fn held(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|e| e > now) && (self.left > 0 || self.usable_now)
    }

    /// Anthropic would take it now, as last read.
    pub fn usable(&self, r: &Resets, now: i64) -> bool {
        self.held(now) && self.usable_now && !self.paused && self.starts_at.is_none_or(|s| s <= now) && r.cooldown_until.is_none_or(|c| c <= now)
    }

    /// What it does, in a sentence's middle: "refills your 5-hour and
    /// weekly limits", or for a Fable reset "gives back to your weekly
    /// limit what Fable used".
    pub fn effect(&self) -> String {
        if self.gives_back_weekly {
            return "gives back to your weekly limit what Fable used".into();
        }
        match self.kind {
            _ if !self.clears.is_empty() => limits_phrase(&self.clears, "refills"),
            Kind::Session => "refills your 5-hour limit".into(),
            Kind::Weekly => "refills your weekly limits".into(),
            _ => "refills your 5-hour and weekly limits".into(),
        }
    }

    /// Why it can't be used now, when it can't.
    pub fn why_not(&self, r: &Resets, now: i64) -> Option<String> {
        if self.usable(r, now) {
            return None;
        }
        let blocked = |k: &str| format!("Can't be used while the account is at its {}.", limit_name(k));
        Some(match (self.starts_at, r.cooldown_until) {
            (Some(s), _) if s > now => format!("Usable from {}.", when(s, now)),
            (_, Some(c)) if c > now => "Another reset was just used on this account: try again in a minute.".into(),
            _ if self.paused => "Paused by Anthropic for now.".into(),
            _ => match self.unavailable.as_deref() {
                Some("cooldown") => "Another reset was just used on this account: try again in a minute.".into(),
                Some("blocked_five_hour") => blocked("five_hour"),
                Some("blocked_seven_day") => blocked("seven_day"),
                Some("blocked_opus") => blocked("seven_day_opus"),
                Some("blocked_sonnet") => blocked("seven_day_sonnet"),
                Some("blocked_other") => "Can't be used while the account is at another limit it doesn't refill.".into(),
                Some("cannot_clear") => "Doesn't refill the limit the account is at.".into(),
                Some("needs_limit") => "Usable once the account reaches a limit it refills.".into(),
                _ if self.needs_limit && !r.at_limit => "Usable once the account reaches a limit it refills.".into(),
                _ => "Anthropic says it can't be used right now.".into(),
            },
        })
    }

    /// Using it now, before reaching a limit it refills, wastes what is
    /// left of that limit: "You still have 60% of your weekly limit left."
    pub fn early(&self, r: &Resets) -> Option<String> {
        if self.needs_limit || self.clears.iter().any(|k| r.exhausted.contains(k)) {
            return None;
        }
        let (key, used) = self.percent_used.iter().filter(|(k, _)| self.clears.contains(k)).max_by(|a, b| a.1.total_cmp(&b.1))?;
        let left = format!("The account still has {}% of its {} left.", (100.0 - used).round().max(0.0), limit_name(key));
        Some(if self.gives_back_weekly { left } else { format!("{left} Using the reset now gives up the rest of it.") })
    }
}

/// When `ms` is, for an expiry: "Dec 10, 3:00 PM (in 23d 4h)", local time.
pub fn when(ms: i64, now: i64) -> String {
    let off = crate::time::local_offset_ms();
    let at = crate::time::day_clock(ms, now, off);
    if ms > now { format!("{at} (in {})", crate::time::until(ms)) } else { at }
}

/// How using a reset went.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Used: what it refilled, and how many of it are left.
    Used { left: Option<u32>, cleared: Vec<String> },
    /// The account wasn't at a limit the reset refills: nothing used.
    NotLimited,
    /// Another reset was just used: nothing used.
    Cooldown,
    /// It had been used already: nothing changed now.
    AlreadyUsed,
    /// It is no longer on offer: nothing used.
    Ineligible,
    /// Anthropic turned it down (nothing used), in its words.
    Refused(String),
    /// Never sent (nothing used): why.
    NotSent(String),
    /// Sent, but whether it went through isn't known: trying again with
    /// the same request id can't use it twice.
    Unconfirmed(String),
}

impl Outcome {
    /// Nothing about it changed the account.
    pub fn nothing_used(&self) -> bool {
        !matches!(self, Outcome::Used { .. } | Outcome::Unconfirmed(_))
    }

    /// What to tell the person, for a reset of `kind`.
    pub fn text(&self, kind: Kind) -> String {
        match self {
            Outcome::Used { left, cleared } => {
                let done = if kind == Kind::Fable {
                    "Fable reset used: what Fable used is back in the weekly limit.".to_string()
                } else if cleared.is_empty() {
                    format!("{} used: the limits it covers are refilled.", kind.name())
                } else {
                    format!("{} used: it {}.", kind.name(), limits_phrase(cleared, "refilled"))
                };
                match left {
                    Some(0) => format!("{done} None of it left."),
                    Some(n) => format!("{done} {n} of it left."),
                    None => done,
                }
            }
            Outcome::NotLimited => "The account wasn't at a limit this reset refills: nothing was used.".into(),
            Outcome::Cooldown => "Another reset was just used on this account: this one wasn't used. Try again in a minute.".into(),
            Outcome::AlreadyUsed => "That reset was already used: nothing changed just now.".into(),
            Outcome::Ineligible => "This reset isn't on offer any more: nothing was used.".into(),
            Outcome::Refused(e) => format!("Anthropic turned the reset down: nothing was used. {e}"),
            Outcome::NotSent(e) => format!("The reset wasn't asked for: nothing was used. {e}"),
            Outcome::Unconfirmed(e) => format!("Couldn't confirm whether the reset went through ({e}). Check the account's limits; trying again can't use it twice."),
        }
    }
}

/// The account's resets as Anthropic says now (its limits too): read
/// afresh unless read in the last `max_age_ms`.
pub fn fresh(id: u64, max_age_ms: i64) -> Result<Resets, String> {
    let now = now_ms();
    if let Some(s) = ACCOUNTS.lock().unwrap().get(id)
        && now - s.resets_read < max_age_ms
        && let Some(r) = &s.resets
    {
        return Ok(r.clone());
    }
    let (limits, resets) = usage_report(id).map_err(|(e, _)| e)?;
    let now = now_ms();
    if let Some(s) = ACCOUNTS.lock().unwrap().get_mut(id) {
        (s.limits, s.error, s.usage_at, s.usage_fails, s.usage_retry_at) = (limits, None, now, 0, 0);
        (s.resets, s.resets_read) = (resets.clone(), now);
    }
    note_polled(id);
    resets.ok_or_else(|| "Anthropic lists no resets for this account.".to_string())
}

/// Uses reset `grant` on account `id`, once: `request_id` (one per
/// confirmation, the same when trying again after an unconfirmed attempt)
/// makes asking twice use it once. Only ever called when the person has
/// confirmed this very reset in Home's dialog: never by an agent, a
/// rotation or a retry of its own.
pub fn use_reset(id: u64, grant: &str, request_id: &str) -> Outcome {
    if !valid_id(grant) || request_id.is_empty() || request_id.len() > 64 || !request_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Outcome::NotSent("The reset's id isn't one Anthropic takes.".into());
    }
    let org = match send_as(id, reqwest::Method::GET, "/api/oauth/profile", &[], false, None).and_then(json_of) {
        Ok(p) => p["organization"]["uuid"].as_str().unwrap_or_default().to_string(),
        Err(e) => return Outcome::NotSent(format!("Couldn't read the account's organization: {e}")),
    };
    if org.is_empty() || !org.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Outcome::NotSent("The account's profile names no organization.".into());
    }
    let body = json!({"program": PROGRAM, "grant_id": grant, "request_id": request_id});
    let path = format!("/api/organizations/{org}/reset_rate_limits");
    let resp = as_account(id, |auth| send_fresh(|| request(reqwest::Method::POST, &path, &[], false, auth).timeout(USE_TIMEOUT).json(&body)));
    let outcome = match resp {
        Err(e) => Outcome::Unconfirmed(e),
        Ok(resp) => read_outcome(resp.status().as_u16(), &resp.text().unwrap_or_default()),
    };
    // Its limits and resets changed (or may have): read them again soon.
    if let Some(s) = ACCOUNTS.lock().unwrap().get_mut(id) {
        s.usage_at = 0;
        s.resets_read = 0;
    }
    outcome
}

/// What reset_rate_limits answered (`status`, `body`) means.
fn read_outcome(status: u16, body: &str) -> Outcome {
    let short = || {
        let v: Option<Value> = serde_json::from_str(body).ok();
        let msg = v.as_ref().and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()).or(v["reason"].as_str())).map(str::to_string);
        format!("HTTP {status}{}", msg.map(|m| format!(": {m}")).unwrap_or_default())
    };
    match status {
        200..=299 => {}
        429 => return Outcome::Refused("Too many requests: try again in a moment.".into()),
        401 | 403 => return Outcome::Refused(format!("The account's sign-in was refused ({}).", short())),
        400..=499 => return Outcome::Refused(short()),
        _ => return Outcome::Unconfirmed(short()),
    }
    let Ok(v) = serde_json::from_str::<Value>(body) else { return Outcome::Unconfirmed("an answer OAK can't read".into()) };
    match v["result"].as_str() {
        Some("reset") => Outcome::Used {
            left: v["resets_left"].as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32),
            cleared: v["cleared"].as_array().map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        },
        Some("not_limited") => Outcome::NotLimited,
        Some("cooldown") => Outcome::Cooldown,
        Some("already_used") => Outcome::AlreadyUsed,
        Some("ineligible") => Outcome::Ineligible,
        Some("unavailable") => Outcome::Unconfirmed("Anthropic said the reset service is unavailable".into()),
        Some(other) => Outcome::Unconfirmed(format!("an answer OAK doesn't know: {other}")),
        None => Outcome::Unconfirmed("an answer without a result".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A usage report with resets, shaped as Claude Code 2.1.322 reads it.
    fn report() -> Value {
        json!({
            "five_hour": {"utilization": 100.0, "resets_at": "2026-11-10T20:00:00+00:00"},
            "seven_day": {"utilization": 85.0, "resets_at": "2026-11-13T08:00:00+00:00"},
            "cedar_ember": {
                "eligible": true,
                "at_limit": true,
                "exhausted": ["five_hour"],
                "weekly_resets_at": "2026-11-13T08:00:00+00:00",
                "cooldown_until": null,
                "grants": [
                    {"id": "full_1", "label": "Full reset", "resets_total": 1, "resets_left": 1, "ends_at": "2026-12-10T22:00:00Z",
                     "clears": ["five_hour", "seven_day"], "usable_now": true, "use_requires_limit": true, "type": "full",
                     "percent_used": {"five_hour": 100, "seven_day": 85}},
                    {"id": "fable_2", "resets_total": 1, "resets_left": 1, "ends_at": "2026-11-17T07:59:00Z",
                     "clears": ["seven_day_overage_included"], "gives_back_weekly": true, "usable_now": false,
                     "use_requires_limit": false, "unavailable_reason": "cannot_clear", "percent_used": {"seven_day_overage_included": 40}},
                    {"id": "used_3", "resets_total": 1, "resets_left": 0, "clears": ["five_hour"], "type": "session"},
                    {"id": "Bad Id", "resets_left": 1}
                ]
            }
        })
    }

    #[test]
    fn reads_the_resets_on_offer() {
        let now = crate::time::parse_time("2026-11-10T18:00:00Z").unwrap();
        let r = read_resets(&report()).unwrap();
        assert!(r.eligible && r.at_limit);
        assert_eq!(r.exhausted, ["five_hour"]);
        assert_eq!(r.grants.len(), 3, "an id reset_rate_limits wouldn't take is dropped");
        let held: Vec<(&str, &str)> = r.held(now).iter().map(|g| (g.id.as_str(), g.kind.name())).collect();
        assert_eq!(held, [("full_1", "Full reset"), ("fable_2", "Fable reset")], "a used-up reset isn't held");
        let full = r.get("full_1", now).unwrap();
        assert_eq!(full.expires_at, crate::time::parse_time("2026-12-10T22:00:00Z"));
        assert!(full.usable(&r, now) && full.why_not(&r, now).is_none());
        assert_eq!(full.effect(), "refills your 5-hour and weekly limits");
        assert!(full.early(&r).is_none(), "it needs a limit, so it is never early");
        let fable = r.get("fable_2", now).unwrap();
        assert_eq!(fable.effect(), "gives back to your weekly limit what Fable used");
        assert!(!fable.usable(&r, now));
        assert_eq!(fable.why_not(&r, now).unwrap(), "Doesn't refill the limit the account is at.");
        assert_eq!(fable.early(&r).unwrap(), "The account still has 60% of its included-usage weekly limit left.");
        assert_eq!((Kind::Fable.lower(), Kind::Full.lower(), Kind::Session.lower()), ("Fable reset".into(), "full reset".into(), "5-hour reset".into()));
        // Expired, it isn't held any more.
        assert!(r.held(crate::time::parse_time("2026-11-20T00:00:00Z").unwrap()).iter().all(|g| g.id != "fable_2"));
        // A cooldown stops every reset.
        let mut cool = r.clone();
        cool.cooldown_until = Some(now + 60_000);
        assert!(!cool.get("full_1", now).unwrap().usable(&cool, now));
        assert!(cool.get("full_1", now).unwrap().why_not(&cool, now).unwrap().contains("just used"));
    }

    #[test]
    fn a_report_without_resets_lists_none() {
        assert_eq!(read_resets(&json!({"five_hour": {"utilization": 3.0}})), None);
        assert_eq!(read_resets(&json!({"cedar_ember": null})), None);
        let r = read_resets(&json!({"cedar_ember": {"eligible": false, "ineligible_reason": "tier"}})).unwrap();
        assert!(!r.eligible && r.grants.is_empty());
        assert_eq!(r.ineligible.as_deref(), Some("tier"));
    }

    #[test]
    fn reads_what_using_a_reset_did() {
        assert_eq!(read_outcome(200, r#"{"result":"reset","resets_left":0,"cleared":["five_hour","seven_day"]}"#), Outcome::Used { left: Some(0), cleared: vec!["five_hour".into(), "seven_day".into()] });
        assert_eq!(read_outcome(200, r#"{"result":"not_limited"}"#), Outcome::NotLimited);
        assert_eq!(read_outcome(200, r#"{"result":"cooldown"}"#), Outcome::Cooldown);
        assert_eq!(read_outcome(200, r#"{"result":"already_used"}"#), Outcome::AlreadyUsed);
        assert_eq!(read_outcome(200, r#"{"result":"ineligible"}"#), Outcome::Ineligible);
        assert!(matches!(read_outcome(200, r#"{"result":"unavailable"}"#), Outcome::Unconfirmed(_)));
        assert!(matches!(read_outcome(200, "<html>"), Outcome::Unconfirmed(_)));
        assert!(matches!(read_outcome(502, ""), Outcome::Unconfirmed(_)), "a server error may have used it");
        assert!(matches!(read_outcome(429, ""), Outcome::Refused(_)));
        assert_eq!(read_outcome(400, r#"{"error":{"message":"bad grant"}}"#), Outcome::Refused("HTTP 400: bad grant".into()));
        assert!(Outcome::NotLimited.nothing_used() && !Outcome::Unconfirmed(String::new()).nothing_used());
        assert_eq!(Outcome::Used { left: Some(0), cleared: vec!["five_hour".into(), "seven_day".into()] }.text(Kind::Full), "Full reset used: it refilled your 5-hour and weekly limits. None of it left.");
    }

    #[test]
    fn never_sends_an_id_anthropic_wouldnt_take() {
        assert!(matches!(use_reset(1, "Bad Id", "r1"), Outcome::NotSent(_)));
        assert!(matches!(use_reset(1, "full_1", "has space"), Outcome::NotSent(_)));
    }
}
