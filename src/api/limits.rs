//! Claude's shared plan windows, model-only quotas and request refusals.
//! A refusal with no scope is evidence about the requested model, not every
//! model on the account. Old account-wide refusals still reconcile against a
//! newer complete shared-usage report (including resets used elsewhere).

use super::{Limit, Stored};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
pub struct ModelRefusal {
    model: String,
    until: i64,
    at: i64,
}

fn family(model: &str) -> &str {
    for f in ["opus", "sonnet", "haiku", "fable", "mythos"] {
        if model == f || model.starts_with(&format!("claude-{f}-")) {
            return if f == "mythos" { "fable" } else { f };
        }
    }
    model
}

/// The model family's name as users see it ("Fable"), None for an unknown model.
pub(super) fn family_name(model: &str) -> Option<&'static str> {
    Some(match family(model) {
        "opus" => "Opus",
        "sonnet" => "Sonnet",
        "haiku" => "Haiku",
        "fable" => "Fable",
        _ => return None,
    })
}

fn scope(label: &str) -> Option<&str> {
    match label {
        "5h" | "Week" => Some(""),
        "Opus wk" => Some("opus"),
        "Sonnet wk" => Some("sonnet"),
        "Haiku wk" => Some("haiku"),
        "Fable wk" => Some("fable"),
        _ => None,
    }
}

impl Limit {
    /// A quota of one model family's ("Fable wk"), not one of the plan's
    /// shared windows that hold every model ("5h", "Week").
    pub fn model_scoped(&self) -> bool {
        scope(self.label).is_some_and(|s| !s.is_empty())
    }
}

/// Whether a plan (as accounts keep it) can go without a 5-hour limit:
/// Ultra ("Max 20x") accounts Anthropic has verified have only weekly ones
/// (claude.com/pricing and the Help Center's "What is the Ultra plan?", since
/// October 30, 2026), and their usage reports and replies carry no 5-hour
/// window. Every other plan's complete reading has both.
pub(super) fn may_lack_five_hour(plan: &str) -> bool {
    plan == "Max 20x"
}

impl Stored {
    pub(super) fn main_limits(&self) -> impl Iterator<Item = &Limit> {
        self.limits_for("")
    }

    fn limits_for<'a>(&'a self, model: &'a str) -> impl Iterator<Item = &'a Limit> {
        self.limits.iter().filter(move |l| scope(l.label).is_some_and(|s| s.is_empty() || (!model.is_empty() && s == family(model))))
    }

    pub(super) fn blocked_until(&self, now: i64) -> i64 {
        self.blocked_for("", now)
    }

    pub(super) fn blocked_for(&self, model: &str, now: i64) -> i64 {
        let used_up = self.limits_for(model)
            .filter(|l| l.percent >= 100.0)
            .map(|l| l.resets_at.unwrap_or(now + 60_000))
            .filter(|&at| at > now).max().unwrap_or(0);
        let refused = self.model_refusals.iter().filter(|r| !model.is_empty() && r.model == family(model)).map(|r| r.until).max().unwrap_or(0);
        let at = used_up.max(self.limited_until).max(refused).max(self.unreachable_until);
        if at > now { at } else { 0 }
    }

    /// A known shared claim holds all models. An absent or unknown claim
    /// holds only the requested family; fresh shared windows still apply.
    pub(super) fn refuse(&mut self, model: &str, claim: &str, until: i64, now: i64) {
        if matches!(claim, "five_hour" | "seven_day" | "5h" | "7d" | "session" | "weekly_all") {
            self.limited_until = self.limited_until.max(until);
            self.limited_at = now;
            return;
        }
        let model = family(model).to_string();
        if let Some(r) = self.model_refusals.iter_mut().find(|r| r.model == model) {
            r.until = r.until.max(until);
            r.at = now;
        } else {
            self.model_refusals.push(ModelRefusal { model, until, at: now });
        }
    }

    pub(super) fn take_limits(&mut self, limits: Vec<Limit>, asked_at: i64, now: i64) -> (bool, bool) {
        let models = ["", "opus", "sonnet", "haiku", "fable"];
        let before = models.map(|m| self.blocked_for(m, now) > 0);
        self.limits = limits;
        // Room takes a complete reading of the shared windows: the weekly
        // one, and the 5-hour one unless the plan may have none.
        let has = |label: &str| self.main_limits().any(|l| l.label == label);
        let complete = has("Week") && (has("5h") || may_lack_five_hour(&self.plan));
        let room = complete && self.main_limits().all(|l| l.percent < 100.0);
        let mut cleared = self.limited_until > now && asked_at > self.limited_at && room;
        if cleared {
            self.limited_until = 0;
        }
        if room && asked_at > self.cloud_limited_at && self.cloud_limited_until > now {
            self.cloud_limited_until = 0;
            cleared = true;
        }
        // Shared room alone cannot clear a Fable refusal. Require a newer
        // reading of that family's own quota as well as both shared windows.
        let limits = &self.limits;
        self.model_refusals.retain(|r| {
            let own: Vec<_> = limits.iter().filter(|l| scope(l.label) == Some(r.model.as_str())).collect();
            let reset = asked_at > r.at && room && !own.is_empty() && own.iter().all(|l| l.percent < 100.0);
            cleared |= reset;
            !reset && r.until > now
        });
        let freed = models.iter().enumerate().any(|(i, m)| before[i] && self.blocked_for(m, now) == 0);
        (cleared, freed)
    }

    pub(super) fn headroom(&self, now: i64) -> f32 {
        self.headroom_for("", now)
    }

    pub(super) fn headroom_for(&self, model: &str, now: i64) -> f32 {
        if self.api_key() { return -1.0; }
        self.limits_for(model)
            .map(|l| if l.resets_at.is_some_and(|at| at <= now) { 100.0 } else { 100.0 - l.percent })
            .reduce(f32::min).unwrap_or(0.0)
    }

    pub(super) fn near_limit(&self, now: i64) -> bool {
        self.main_limits().any(|l| l.resets_at.is_none_or(|at| at > now) && l.percent >= if l.label == "5h" { super::SOON_5H } else { super::SOON_WEEK })
    }
}
