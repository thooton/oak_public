//! Provider model lists, names, context windows, output limits and effort
//! capabilities, cached in models.json.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub provider: Provider,
}

/// Who serves a model (`providers::Kind`).
pub use crate::providers::Kind as Provider;

/// Why the Claude models couldn't be listed, when the OpenRouter ones
/// could (the list then has those only).
static CLAUDE_MODELS_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn claude_models_error() -> Option<String> {
    CLAUDE_MODELS_ERROR.lock().unwrap().clone()
}

/// What the model lists said, kept in models.json in OAK's folder so it
/// is there from the start (the subagents' model choice is part of the
/// tool definitions, which must not change within a conversation).
#[derive(Default, Serialize, Deserialize)]
struct ModelCache {
    /// The Claude models to pick from (newest Opus, Sonnet, Haiku, Fable).
    #[serde(default)]
    models: Vec<Model>,
    /// The most each model may write in one reply (`max_tokens`), and its
    /// context window (`max_input_tokens`), by model id: every model the
    /// account has, for conversations on older ones.
    #[serde(default)]
    max_output: std::collections::HashMap<String, u64>,
    #[serde(default)]
    context: std::collections::HashMap<String, u64>,
    /// The effort levels each model takes (`capabilities.effort`), by
    /// model id: empty for one without effort.
    #[serde(default)]
    efforts: std::collections::HashMap<String, Vec<String>>,
}

static KNOWN: std::sync::LazyLock<Mutex<ModelCache>> = std::sync::LazyLock::new(|| {
    Mutex::new(crate::store::read_json(&crate::store::dir().join("models.json")).unwrap_or_default())
});

fn save_known(k: &ModelCache) {
    let _ = crate::store::write_json(&crate::store::dir().join("models.json"), k);
}

/// A model in one of the providers' own lists, as far as the lookups here
/// need it. Freebuff's list has no `max_output` (its replies are bounded by
/// nothing else), so that one is a `None`.
struct Sale {
    name: &'static str,
    context: u64,
    max_output: Option<u64>,
}

/// What a provider's own list says about `model`, if it lists it.
fn sale(model: &str) -> Option<Sale> {
    let listed = |id: &str, name: &'static str, context: u64, max_output: u64| {
        let _ = id;
        Sale { name, context, max_output: Some(max_output) }
    };
    match Provider::of(model) {
        Provider::OpenRouter => crate::openrouter::model(model).map(|m| listed(m.id, m.name, m.context, m.max_output)),
        Provider::Codex => crate::codex::model(model).map(|m| listed(m.id, m.name, m.context, m.max_output)),
        Provider::NodeClub => crate::nodeclub::model(model).map(|m| listed(m.id, m.name, m.context, m.max_output)),
        Provider::Freebuff => crate::freebuff::model(model).map(|m| Sale { name: m.name, context: m.context, max_output: None }),
        _ => None,
    }
}

/// The models a conversation (or a subagent) can use: the Claude ones as
/// last listed, then each provider's that is signed in, in the pickers'
/// order (`Provider::ALL`).
pub fn known_models() -> Vec<Model> {
    let mut out: Vec<Model> = KNOWN.lock().unwrap().models.iter().filter(|m| m.provider == Provider::Claude).cloned().collect();
    for provider in Provider::ALL {
        out.extend(provider_models(provider));
    }
    out
}

/// The models of a provider OAK has a login for, in its own list's order;
/// none for Claude (those come from the API) or a provider signed out.
fn provider_models(provider: Provider) -> Vec<Model> {
    let listed = |id: &'static str, name: &'static str| Model { id: id.into(), name: name.into(), provider };
    if provider == Provider::Claude || !provider.on() {
        return vec![];
    }
    match provider {
        Provider::OpenRouter => crate::openrouter::MODELS.iter().map(|m| listed(m.id, m.name)).collect(),
        Provider::Codex => crate::codex::MODELS.iter().filter(|m| m.listed).map(|m| listed(m.id, m.name)).collect(),
        Provider::Freebuff => crate::freebuff::MODELS.iter().map(|m| listed(m.id, m.name)).collect(),
        Provider::Local => crate::local::model_id().map(|id| Model { name: crate::local::model_name(&id), id, provider }).into_iter().collect(),
        _ => crate::nodeclub::MODELS.iter().map(|m| listed(m.id, m.name)).collect(),
    }
}

/// A model's name ("Claude Opus 5.5"), as far as it is known; else its id.
pub fn model_name(id: &str) -> String {
    let known = sale(id).map(|m| m.name.to_string());
    if let Some(name) = known.or_else(|| crate::local::serves(id).then(|| crate::local::model_name(id))) {
        return name;
    }
    KNOWN.lock().unwrap().models.iter().find(|m| m.id == id).map_or(id.to_string(), |m| m.name.clone())
}

/// The model's context window (tokens), when known.
pub fn context_window(model: &str) -> Option<u64> {
    sale(model).map(|m| m.context).or_else(|| crate::local::serves(model).then(|| crate::local::context(model))).or_else(|| KNOWN.lock().unwrap().context.get(model).copied())
}

/// The effort levels a Claude model takes, as the Models API last listed
/// them (empty: it has no effort setting); None until it has.
pub fn claude_efforts(model: &str) -> Option<Vec<String>> {
    KNOWN.lock().unwrap().efforts.get(model).cloned()
}

/// The levels in a model's `capabilities.effort` that say they are
/// supported; None when the model lists no capabilities.
fn effort_levels(model: &Value) -> Option<Vec<String>> {
    let effort = model["capabilities"]["effort"].as_object()?;
    if effort.get("supported").and_then(Value::as_bool) != Some(true) {
        return Some(vec![]);
    }
    let on = |v: &Value| v["supported"].as_bool() == Some(true);
    Some(crate::agent::EFFORTS.iter().filter(|l| effort.get(**l).is_some_and(on)).map(|l| l.to_string()).collect())
}

/// What Claude Code asks for when it doesn't know a model's limit, and
/// what every current model takes.
pub const DEFAULT_MAX_OUTPUT: u64 = 32_000;

/// `max_tokens` for a request: the model's own maximum, so replies are
/// never cut short by OAK (Claude 4.5 and newer models take it even when
/// it and the prompt together pass the context window: the reply then
/// stops at the window). Until the model list has come in, Claude Code's
/// default for models it doesn't know.
pub fn max_output(model: &str) -> u64 {
    let own = sale(model).and_then(|m| m.max_output).or_else(|| crate::local::serves(model).then(|| crate::local::max_output(model)));
    own.unwrap_or_else(|| KNOWN.lock().unwrap().max_output.get(model).copied().unwrap_or(DEFAULT_MAX_OUTPUT))
}

/// The models to pick from: the newest Opus, Sonnet, Haiku and Fable the
/// Claude account can use, then those of every other provider signed in. With
/// those and no Claude account (or the Claude list failing), those only.
pub fn latest_models() -> Result<Vec<Model>, String> {
    let extra: Vec<Model> = Provider::ALL.into_iter().flat_map(provider_models).collect();
    let claude = if current_id().is_err() && !extra.is_empty() { Ok(vec![]) } else { claude_models() };
    let (list, error) = match claude {
        Ok(mut list) => {
            if !list.is_empty() {
                let mut k = KNOWN.lock().unwrap();
                k.models = list.clone();
                save_known(&k);
            }
            list.extend(extra);
            (Ok(list), None)
        }
        Err(e) if !extra.is_empty() => (Ok(extra), Some(e)),
        Err(e) => (Err(e), None),
    };
    *CLAUDE_MODELS_ERROR.lock().unwrap() = error;
    list
}

/// The newest Opus, Sonnet, Haiku and Fable the account can use.
fn claude_models() -> Result<Vec<Model>, String> {
    let v = json_of(send_as(current_id()?, reqwest::Method::GET, "/v1/models?limit=1000", &[], false, None)?)?;
    let empty = vec![];
    let all = v["data"].as_array().unwrap_or(&empty);
    {
        let mut k = KNOWN.lock().unwrap();
        for m in all {
            let Some(id) = m["id"].as_str() else { continue };
            if let Some(n) = m["max_tokens"].as_u64().filter(|&n| n > 0) {
                k.max_output.insert(id.to_string(), n);
            }
            if let Some(n) = m["max_input_tokens"].as_u64().filter(|&n| n > 0) {
                k.context.insert(id.to_string(), n);
            }
            if let Some(levels) = effort_levels(m) {
                k.efforts.insert(id.to_string(), levels);
            }
        }
    }
    let newest = |family: &str| {
        all.iter()
            .filter(|m| m["id"].as_str().is_some_and(|id| id.contains(family)))
            .max_by_key(|m| m["created_at"].as_str().unwrap_or(""))
            .map(|m| {
                let id = m["id"].as_str().unwrap().to_string();
                let name = m["display_name"].as_str().unwrap_or(&id).to_string();
                Model { id, name, provider: Provider::Claude }
            })
    };
    Ok(["opus", "sonnet", "haiku", "fable"].into_iter().filter_map(newest).collect())
}

/// When Fable 5.2's launch offer stops including the Fable models in the
/// limits of Pro plans and Team Standard seats (ms since 1970): it runs
/// from launch (November 9, 2026) through November 16, 2026 at 11:59 PM
/// PT, as support.claude.com/en/articles/17506494 ("Claude Fable 5.2
/// launch offers") says. Outside it those plans pay for Fable with usage
/// credits; Max, Ultra and Team Premium include it in their limits for
/// good (support.claude.com/en/articles/15424964).
pub const FABLE_INCLUDED_UNTIL: i64 = 1_794_902_400_000;

/// `FABLE_INCLUDED_UNTIL` as the pickers say it.
pub const FABLE_INCLUDED_UNTIL_TEXT: &str = "Nov 16, 2026, 11:59 PM PT";

/// How a Claude plan (`Account::plan`) pays for the Fable models.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FableTerms {
    /// A standard part of the plan's limits (up to half of the week's).
    Included,
    /// In the plan's limits until `FABLE_INCLUDED_UNTIL`, then on usage
    /// credits.
    LimitedTime,
    /// Neither: usage credits, API rates, or a plan OAK can't tell.
    Other,
}

pub fn fable_terms(plan: &str) -> FableTerms {
    match plan {
        "Pro" | "Team Standard" => FableTerms::LimitedTime,
        "Team Premium" => FableTerms::Included,
        p if p.starts_with("Max") || p.starts_with("Ultra") => FableTerms::Included,
        _ => FableTerms::Other,
    }
}

/// Whether `id` is one of the Claude Fable models (Fable or Mythos).
pub fn is_fable(id: &str) -> bool {
    id.starts_with("claude-fable-") || id.starts_with("claude-mythos-")
}

/// What the pickers say about Fable model `id` on the Claude accounts'
/// plans at `now` (ms): a few words for its label (empty when there's
/// nothing limited about it), and the whole story for its tooltip. None
/// for any other model, or with no Claude plan known.
pub fn fable_offer(id: &str, accounts: &[Account], now: i64) -> Option<(String, String)> {
    if !is_fable(id) {
        return None;
    }
    let plans: Vec<(&Account, FableTerms)> = accounts.iter().filter(|a| a.provider == Provider::Claude && !a.api_key && !a.plan.is_empty()).map(|a| (a, fable_terms(&a.plan))).collect();
    let who = |terms: FableTerms| -> Vec<String> {
        plans.iter().filter(|(_, t)| *t == terms).map(|(a, _)| if a.email.is_empty() { a.plan.clone() } else { format!("{} ({})", a.email, a.plan) }).collect()
    };
    let (limited, included) = (who(FableTerms::LimitedTime), who(FableTerms::Included));
    if limited.is_empty() && included.is_empty() {
        return None;
    }
    let open = now < FABLE_INCLUDED_UNTIL;
    let label = match () {
        _ if limited.is_empty() => String::new(),
        _ if open => format!("Included until {FABLE_INCLUDED_UNTIL_TEXT}"),
        _ if included.is_empty() => "usage credits only".to_string(),
        _ => "usage credits on Pro".to_string(),
    };
    let mut tip = vec![];
    if !limited.is_empty() {
        tip.push(if open {
            format!("Included until {FABLE_INCLUDED_UNTIL_TEXT} on Pro and Team Standard (Fable 5.2's launch offer: up to half of the weekly limit). After that, Fable models there take usage credits.\n{}", limited.join("\n"))
        } else {
            format!("Fable 5.2's launch offer ended {FABLE_INCLUDED_UNTIL_TEXT}: on Pro and Team Standard, Fable models take usage credits.\n{}", limited.join("\n"))
        });
    }
    if !included.is_empty() {
        tip.push(format!("Included in the plan's limits for good on Max, Ultra and Team Premium (up to half of the weekly limit).\n{}", included.join("\n")));
    }
    Some((label, tip.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(email: &str, plan: &str) -> Account {
        Account { email: email.into(), plan: plan.into(), ..Default::default() }
    }

    /// Fable's terms follow each plan: Pro and Team Standard only until the
    /// launch offer ends (Nov 16, 2026, 11:59 PM PT), Max for good.
    #[test]
    fn fable_offer_follows_the_plans() {
        let before = FABLE_INCLUDED_UNTIL - 1;
        assert_eq!(fable_terms("Pro"), FableTerms::LimitedTime);
        assert_eq!(fable_terms("Team Standard"), FableTerms::LimitedTime);
        assert_eq!(fable_terms("Max 20x"), FableTerms::Included);
        assert_eq!(fable_terms("Team Premium"), FableTerms::Included);
        assert_eq!(fable_terms("claude_enterprise"), FableTerms::Other);

        let pro = [acct("a@x.dev", "Pro")];
        let (label, tip) = fable_offer("claude-fable-5-2", &pro, before).unwrap();
        assert_eq!(label, "Included until Nov 16, 2026, 11:59 PM PT");
        assert!(tip.contains("a@x.dev (Pro)") && tip.contains("usage credits"));
        assert_eq!(fable_offer("claude-fable-5-2", &pro, FABLE_INCLUDED_UNTIL).unwrap().0, "usage credits only");

        let max = [acct("b@x.dev", "Max 5x")];
        let (label, tip) = fable_offer("claude-fable-5-2", &max, before).unwrap();
        assert!(label.is_empty() && tip.contains("for good"));

        let both = [pro[0].clone(), max[0].clone()];
        assert_eq!(fable_offer("claude-fable-5-2", &both, before).unwrap().0, "Included until Nov 16, 2026, 11:59 PM PT");
        assert_eq!(fable_offer("claude-fable-5-2", &both, FABLE_INCLUDED_UNTIL).unwrap().0, "usage credits on Pro");

        // Not Fable, or no plan known (an API key): nothing to say.
        assert!(fable_offer("claude-opus-5-5", &pro, before).is_none());
        assert!(fable_offer("claude-fable-5-2", &[Account { api_key: true, ..acct("", "") }], before).is_none());
    }
}
