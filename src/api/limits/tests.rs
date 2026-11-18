use crate::api::{self, Auth, Limit, Provider, Stored};
use serde_json::json;

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn account(id: u64, five: f32, week: f32, fable: f32) -> Stored {
    let mut a = Stored::new(id, Auth::Oauth { access: "access".into(), refresh: "refresh".into(), expires_at: NOW + HOUR });
    a.limits = report(five, week, fable);
    a
}

fn report(five: f32, week: f32, fable: f32) -> Vec<Limit> {
    // Actual November 2026 report shape: Fable's id is null, and only
    // display_name identifies the model. Its quota has no top-level window.
    api::read_limits(&json!({
        "five_hour": {"utilization":five,"resets_at":"2027-01-16T09:00:00Z"},
        "seven_day": {"utilization":week,"resets_at":"2027-01-20T09:00:00Z"},
        "limits": [
            {"kind":"session","percent":five,"scope":null},
            {"kind":"weekly_all","percent":week,"scope":null},
            {"kind":"weekly_scoped","percent":fable,"resets_at":"2027-01-20T09:00:00Z",
             "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null}}
        ]
    }))
}

#[test]
fn fable_quota_does_not_consume_other_models_room() {
    let mut list = vec![account(0, 2.0, 64.0, 100.0)];
    list.extend((1..4).map(|i| account(i, 100.0, 40.0, 50.0)));
    for m in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"] {
        let c = api::candidates_for(&list, m, NOW);
        assert_eq!(c.iter().map(|c| c.usable).collect::<Vec<_>>(), [true, false, false, false]);
        assert_eq!(c[0].left, 36.0);
        crate::rotation::used(Provider::Claude, m, "1", HOUR, NOW);
        assert_eq!(crate::rotation::pick(Provider::Claude, m, &c, NOW, true).as_deref(), Some("1"));
        assert_eq!(crate::rotation::pick(Provider::Claude, m, &c, NOW, false).as_deref(), Some("0"));
    }
    for m in ["claude-fable-5-2", "claude-mythos-5-2"] {
        assert!(api::candidates_for(&list, m, NOW).iter().all(|c| !c.usable));
    }
    assert!(list[0].limits.iter().any(|l| l.label == "Fable wk" && l.percent == 100.0));
    list[0].limits[0].percent = 100.0;
    assert!(api::candidates_for(&list, "claude-sonnet-5-5", NOW).iter().all(|c| !c.usable));
}

#[test]
fn refusals_are_per_family_and_per_account_and_survive_restart() {
    let mut a = account(0, 2.0, 64.0, 30.0);
    a.refuse("claude-fable-5-2", "", NOW + HOUR, NOW);
    for m in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"] {
        assert_eq!(a.blocked_for(m, NOW), 0);
    }
    assert_eq!(a.blocked_for("claude-fable-5-2", NOW), NOW + HOUR);
    assert_eq!(account(1, 2.0, 64.0, 30.0).blocked_for("claude-fable-5-2", NOW), 0);
    let mut loaded: Stored = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
    assert_eq!(loaded.blocked_for("claude-fable-5-2", NOW), NOW + HOUR);
    assert_eq!(loaded.blocked_for("claude-opus-5-5", NOW), 0);
    // Both shared windows below 100 do NOT clear a model quota refusal.
    let shared = report(2.0, 64.0, 30.0)[..2].to_vec();
    assert_eq!(loaded.take_limits(shared, NOW + 1, NOW), (false, false));
    assert_eq!(loaded.take_limits(report(2.0, 64.0, 100.0), NOW + 1, NOW), (false, false));
    assert_eq!(loaded.take_limits(report(2.0, 64.0, 10.0), NOW - 1, NOW), (false, false));
    assert_eq!(loaded.take_limits(report(2.0, 64.0, 10.0), NOW + 1, NOW), (true, true));
    assert_eq!(loaded.blocked_for("claude-fable-5-2", NOW), 0);
    a.refuse("claude-fable-5-2", "five_hour", NOW + HOUR, NOW);
    assert_eq!(a.blocked_for("claude-sonnet-5-5", NOW), NOW + HOUR);
}

#[test]
fn stale_legacy_refusal_clears_without_unlocking_fable_or_login_failures() {
    let mut a = account(0, 2.0, 64.0, 100.0);
    a.limited_until = NOW + HOUR;
    a.limited_at = NOW;
    assert_eq!(a.take_limits(report(2.0, 64.0, 100.0), NOW - 1, NOW), (false, false));
    assert_eq!(a.take_limits(report(2.0, 64.0, 100.0), NOW + 1, NOW), (true, true));
    assert_eq!(a.blocked_for("claude-opus-5-5", NOW), 0);
    assert!(a.blocked_for("claude-fable-5-2", NOW) > NOW);
    a.unreachable_until = NOW + HOUR;
    assert!(a.blocked_for("claude-opus-5-5", NOW) > NOW);
    assert!(matches!(a.auth, Auth::Oauth { .. }));
}

#[test]
fn unknown_or_missing_scope_is_never_invented_as_shared() {
    let limits = api::read_limits(&json!({"limits":[
        {"kind":"weekly_scoped","percent":100,"scope":{"model":{"id":"claude-fable-5-2"}}},
        {"kind":"weekly_scoped","percent":100,"scope":{"model":{"display_name":"Other"}}},
        {"kind":"weekly_scoped","percent":100,"scope":{"surface":"cowork"}}
    ]}));
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].label, "Fable wk");
    let mut a = account(0, 2.0, 64.0, 100.0);
    // No reset timestamp is not permission to send Fable requests.
    a.limits.iter_mut().find(|l| l.label == "Fable wk").unwrap().resets_at = None;
    assert!(a.blocked_for("fable", NOW) > NOW);
    // Natural expiry and cloud-only refusals don't poison desktop models.
    a.limits.iter_mut().find(|l| l.label == "Fable wk").unwrap().resets_at = Some(NOW - 1);
    a.cloud_limited_until = NOW + HOUR;
    assert_eq!(a.blocked_for("fable", NOW), 0);
}

/// The headers of a real 1-token Fable request through a Max account at
/// 7% / 64% with its Fable allowance used up (November 2026): Anthropic
/// refuses it on its own claim, while Opus on the same account succeeds.
fn max_fable_refusal() -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    for (k, v) in [
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
        ("anthropic-ratelimit-unified-reset", "1795032000"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.07"),
        ("anthropic-ratelimit-unified-5h-reset", "1794819000"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.64"),
        ("anthropic-ratelimit-unified-7d-reset", "1795032000"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.01"),
        ("anthropic-ratelimit-unified-7d_oi-reset", "1795032000"),
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled"),
        ("retry-after", "224990"),
    ] {
        h.insert(k, v.parse().unwrap());
    }
    h
}

#[test]
fn max_fable_allowance_is_enforced_and_named_not_skipped() {
    const LIVE: i64 = 1_794_810_600_000; // when the request was made
    let h = max_fable_refusal();
    let at = api::usage_limit_reset(429, &h, LIVE).unwrap();
    assert_eq!(at, 1_795_032_000_000);
    let mut max = account(0, 7.0, 64.0, 30.0);
    max.plan = "Max 20x".into();
    max.refuse("claude-fable-5-2", h["anthropic-ratelimit-unified-representative-claim"].to_str().unwrap(), at, LIVE);
    assert_eq!(max.blocked_for("claude-fable-5-2", LIVE), at);
    assert_eq!(max.blocked_for("claude-opus-5-5", LIVE), 0);
    assert_eq!(api::model_only_limit([&max].into_iter(), "claude-fable-5-2", LIVE), Some("Fable"));
    assert_eq!(api::model_only_limit([&max].into_iter(), "claude-opus-5-5", LIVE), None);
    // A plan-wide window used up is not called Fable's.
    let mut spent = account(1, 100.0, 64.0, 100.0);
    spent.plan = "Max 5x".into();
    assert_eq!(api::model_only_limit([&spent].into_iter(), "claude-fable-5-2", NOW), None);
}

#[test]
fn an_account_with_fable_room_is_tried_whatever_its_plan() {
    // The reported limits (Fable 100% on all four) leave none for Fable…
    let mut list = vec![account(0, 6.0, 64.0, 100.0), account(1, 3.0, 76.0, 100.0), account(2, 52.0, 86.0, 100.0), account(3, 100.0, 68.0, 100.0)];
    for (s, plan) in list.iter_mut().zip(["Max 20x", "Pro", "Pro", "Pro"]) {
        s.plan = plan.into();
    }
    assert!(api::candidates_for(&list, "claude-fable-5-2", NOW).iter().all(|c| !c.usable));
    assert_eq!(api::model_only_limit(list.iter(), "claude-fable-5-2", NOW), Some("Fable"));
    assert_eq!(api::candidates_for(&list, "claude-opus-5-5", NOW).iter().map(|c| c.usable).collect::<Vec<_>>(), [true, true, true, false]);
    // …and once the Max's allowance is back, it is picked for Fable, not a Pro.
    list[0].limits = report(6.0, 64.0, 0.0);
    let c = api::candidates_for(&list, "claude-fable-5-2", NOW);
    assert_eq!(c.iter().map(|c| c.usable).collect::<Vec<_>>(), [true, false, false, false]);
    assert_eq!(crate::rotation::pick(Provider::Claude, "conv-fable", &c, NOW, false).as_deref(), Some("0"));
}

/// A verified Ultra's usage report has no 5-hour window (Claude Code shows
/// no "Current session" bar for it): its weekly window alone is a complete
/// reading, so a refusal clears early on it, scoped ones as before. Another
/// plan's reading without the 5-hour window says nothing.
#[test]
fn a_verified_ultra_clears_on_its_weekly_window_alone() {
    let verified = |week: f32, fable: f32| {
        api::read_limits(&json!({
            "five_hour": null,
            "seven_day": {"utilization":week,"resets_at":"2027-01-20T09:00:00Z"},
            "limits": [
                {"kind":"weekly_all","percent":week,"scope":null},
                {"kind":"weekly_scoped","percent":fable,"resets_at":"2027-01-20T09:00:00Z",
                 "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null}}
            ]
        }))
    };
    assert_eq!(verified(40.0, 10.0).iter().map(|l| l.label).collect::<Vec<_>>(), ["Week", "Fable wk"]);
    let refused = |plan: &str| {
        let mut a = account(0, 0.0, 100.0, 100.0);
        a.limits = verified(100.0, 100.0);
        a.plan = plan.into();
        a.refuse("claude-opus-5-5", "seven_day", NOW + HOUR, NOW);
        a.refuse("claude-fable-5-2", "seven_day_overage_included", NOW + HOUR, NOW);
        a
    };
    let mut ultra = refused("Max 20x");
    assert!(ultra.blocked_for("claude-opus-5-5", NOW) > NOW);
    assert_eq!(ultra.take_limits(verified(100.0, 100.0), NOW + 1, NOW), (false, false));
    // Its week reset (Fable's still used up): other models are free again.
    assert_eq!(ultra.take_limits(verified(0.0, 100.0), NOW + 1, NOW), (true, true));
    assert_eq!(ultra.blocked_for("claude-opus-5-5", NOW), 0);
    assert!(ultra.blocked_for("claude-fable-5-2", NOW) > NOW);
    assert_eq!(ultra.take_limits(verified(0.0, 0.0), NOW + 1, NOW), (true, true));
    assert_eq!(ultra.blocked_for("claude-fable-5-2", NOW), 0);
    // A Max (or Pro) reading lacking its 5-hour window is incomplete.
    let mut max = refused("Max 5x");
    assert_eq!(max.take_limits(verified(0.0, 0.0), NOW + 1, NOW), (false, false));
    assert_eq!(max.blocked_for("claude-opus-5-5", NOW), NOW + HOUR);
    // An Ultra not yet verified has a 5-hour window, and it still counts.
    let mut unverified = refused("Max 20x");
    assert_eq!(unverified.take_limits(report(100.0, 0.0, 0.0), NOW + 1, NOW), (false, false));
    assert_eq!(unverified.take_limits(report(5.0, 0.0, 0.0), NOW + 1, NOW), (true, true));
}

/// The 5-hour and weekly percents and resets of `limits`, as they hold.
fn held(limits: &[Limit], now: i64) -> Vec<(&'static str, f32, Option<i64>)> {
    api::effective_limits(limits, now).iter().map(|l| (l.label, l.percent, l.resets_at)).collect()
}

/// An account whose week is used up has no 5-hour room either, whatever
/// that window reads, until the week resets: at 99% of the week the 5-hour
/// window reads as it is. The readings themselves are left as sent.
#[test]
fn a_used_up_week_holds_the_five_hour_window_until_it_resets() {
    let (five, week) = (Some(1_800_000_000_000 + 2 * HOUR), Some(1_800_000_000_000 + 50 * HOUR));
    let reading = |five_pct: f32, week_pct: f32, fable: f32| {
        vec![
            Limit { label: "5h", percent: five_pct, resets_at: five },
            Limit { label: "Week", percent: week_pct, resets_at: week },
            Limit { label: "Fable wk", percent: fable, resets_at: week },
        ]
    };
    // Week at 100% (as johnny's Max read, 0% of its 5 hours): both full,
    // the 5-hour one until the week's reset.
    let full = reading(0.0, 100.0, 30.0);
    assert_eq!(held(&full, NOW), [("5h", 100.0, week), ("Week", 100.0, week), ("Fable wk", 30.0, week)]);
    assert!(api::five_hour_held_by_week(&full, NOW));
    assert_eq!(full[0].percent, 0.0);
    // Past the 5-hour window's own reset it stays full.
    assert_eq!(held(&full, NOW + 3 * HOUR)[0], ("5h", 100.0, week));
    // Once the week has reset, the 5-hour window is its own again.
    assert_eq!(held(&full, NOW + 51 * HOUR)[0], ("5h", 0.0, five));
    // At 99% of the week, nothing changes.
    let almost = reading(3.0, 99.0, 30.0);
    assert_eq!(held(&almost, NOW), [("5h", 3.0, five), ("Week", 99.0, week), ("Fable wk", 30.0, week)]);
    assert!(!api::five_hour_held_by_week(&almost, NOW));
    // Fable's own week used up holds Fable alone: the shared 5-hour window
    // reads as it is.
    let fable = reading(3.0, 40.0, 100.0);
    assert_eq!(held(&fable, NOW)[0], ("5h", 3.0, five));
    assert!(!api::five_hour_held_by_week(&fable, NOW));
    // A 5-hour window already full past the week's reset holds to its own.
    let later = vec![Limit { label: "5h", percent: 100.0, resets_at: Some(NOW + 60 * HOUR) }, Limit { label: "Week", percent: 100.0, resets_at: week }];
    assert_eq!(held(&later, NOW)[0], ("5h", 100.0, Some(NOW + 60 * HOUR)));
    assert!(!api::five_hour_held_by_week(&later, NOW));
    // A verified Ultra has no 5-hour window, and isn't given one.
    let ultra = vec![Limit { label: "Week", percent: 100.0, resets_at: week }];
    assert_eq!(held(&ultra, NOW), [("Week", 100.0, week)]);
}

/// Rotation already holds such an account until its week resets: the
/// 5-hour window's reset frees nothing, Fable's own week holds only Fable.
#[test]
fn a_five_hour_reset_does_not_free_a_used_up_week() {
    let mut a = account(0, 0.0, 100.0, 20.0);
    let week = a.limits.iter().find(|l| l.label == "Week").and_then(|l| l.resets_at).unwrap();
    let five = a.limits.iter().find(|l| l.label == "5h").and_then(|l| l.resets_at).unwrap();
    for m in ["", "claude-opus-5-5", "claude-fable-5-2"] {
        assert_eq!(a.blocked_for(m, NOW), week);
        assert_eq!(a.blocked_for(m, five + 1), week);
        assert_eq!(a.headroom_for(m, NOW), 0.0);
    }
    // Refused on the 5-hour claim, then read again with the 5-hour window
    // empty and the week still used up: still held, until the week.
    a.refuse("claude-opus-5-5", "five_hour", five, NOW);
    assert_eq!(a.take_limits(report(0.0, 100.0, 20.0), NOW + 1, NOW), (false, false));
    assert_eq!(a.blocked_for("claude-opus-5-5", five + 1), week);
    let fable = account(1, 0.0, 30.0, 100.0);
    assert_eq!(fable.blocked_for("claude-opus-5-5", NOW), 0);
    assert!(fable.blocked_for("claude-fable-5-2", NOW) > NOW);
}
