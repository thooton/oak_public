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
