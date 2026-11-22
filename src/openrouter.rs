//! OpenRouter: models from other labs (DeepSeek), through OpenRouter's
// Anthropic-format Messages endpoint. The body goes as OAK sends it to
// Claude, less what only Anthropic's gateway wants; the stream comes back as
// the same events. It is paid per token from the account's credits: each
// reply's cost comes with its `message_delta` usage (`cost`, in dollars).
// The keys are kept in openrouter.json (and OPENROUTER_API_KEY), or on the
// vault, for every OAK (`provider::Keys`); each conversation goes
// through one (`crate::rotation`).

use crate::api::{self, Sse};
use crate::oak::{Event, Request};
use crate::provider;
use serde_json::{Value, json};
use std::sync::{LazyLock, Mutex};
use std::sync::mpsc::Sender;
use std::time::Duration;

const BASE: &str = "https://openrouter.ai/api/v1";

/// A model OAK offers through OpenRouter.
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
    /// Sent as `max_tokens`. OpenRouter only routes to providers that can
    /// write that much, so this is kept below the model's own limit when
    /// the cheaper providers stop short of it.
    pub max_output: u64,
    /// Its context window (tokens).
    pub context: u64,
    /// The most a provider may charge (dollars per million input and
    /// output tokens): well under the model maker's own price, so requests
    /// go to the cheap full-precision providers only...
    pub max_price: Option<(f64, f64)>,
    /// ...unless none of those can take a request: then it goes again with
    /// the maker's own price as the ceiling. None for both: no ceiling (a
    /// model added in Settings, whose prices OAK doesn't know).
    pub list_price: Option<(f64, f64)>,
    /// The effort levels it tells apart (`agent::efforts`).
    pub efforts: &'static [&'static str],
    /// It thinks (OpenRouter's `reasoning` parameter). When not, requests
    /// go without thinking or effort, and providers needn't take them.
    pub reasoning: bool,
}

/// The OpenRouter models in the model picker, before those added in
/// Settings (`set_added`); DeepSeek first (the summarizer's, `rolling`).
/// DeepSeek V4.1 Flash 1017 writes up to 393k tokens, but several of the
/// cheapest full-precision providers stop at 131k; no reply needs more.
/// Its ceiling of $0.15/$0.60 lets in Morph, DeepInfra and InferenceNet
/// (fp8, $0.04–0.15 in, $0.24–0.55 out, October 2026), and keeps out those
/// at or near list price (BaseTen at $0.30/$1.20, DigitalOcean, Cohere,
/// Particle AI). Gemma 4 31B's providers stop at 8k to 262k: 32k keeps
/// most of them; its ceiling of $0.15/$0.40 lets in all but the dearest
/// (Modal, SambaNova, ModelRun, SiliconFlow; November 2026). The free ones
/// have one provider each (Google AI Studio, Particle AI, NVIDIA), capped
/// at $0 so a request never falls through to a paid one.
pub const MODELS: &[Model] = &[
    Model {
        id: "deepseek/deepseek-v4.1-flash-1017",
        name: "DeepSeek V4.1 Flash 1017",
        max_output: 131_072,
        context: 1_048_576,
        max_price: Some((0.15, 0.60)),
        list_price: Some((0.30, 1.20)),
        // Its reasoning budget out of 100: low 50, high 75 (its default), max
        // 100 (the model card's encoding notes).
        efforts: &["low", "high", "max"],
        reasoning: true,
    },
    Model {
        id: "google/gemma-4-31b-it",
        name: "Gemma 4 31B",
        max_output: 32_768,
        context: 262_144,
        max_price: Some((0.15, 0.40)),
        list_price: Some((0.20, 0.60)),
        // Thinking on or off, no levels.
        efforts: &[],
        reasoning: true,
    },
    Model {
        id: "google/gemma-4-31b-it:free",
        name: "Gemma 4 31B (free)",
        max_output: 32_768,
        context: 262_144,
        max_price: Some((0.0, 0.0)),
        list_price: Some((0.0, 0.0)),
        efforts: &[],
        reasoning: true,
    },
    Model {
        id: "particle-ai/particle-1:free",
        name: "Particle-1 (free)",
        max_output: 131_072,
        context: 1_048_576,
        max_price: Some((0.0, 0.0)),
        list_price: Some((0.0, 0.0)),
        efforts: &["low", "medium", "high"],
        reasoning: true,
    },
    Model {
        id: "nvidia/nemotron-3.5-super-vl:free",
        name: "Nemotron 3.5 Super VL (free)",
        max_output: 131_072,
        context: 1_000_000,
        max_price: Some((0.0, 0.0)),
        list_price: Some((0.0, 0.0)),
        efforts: &[],
        reasoning: true,
    },
];

/// A model added to the picker in Settings (kept in settings.json as
/// `or_models`), as OpenRouter's list described it when it was added.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Added {
    pub id: String,
    pub name: String,
    pub context: u64,
    pub max_output: u64,
    /// Free (a `:free` id, or $0 in and out): capped at $0.
    pub free: bool,
    pub reasoning: bool,
    /// It takes `reasoning_effort`: low, medium and high.
    pub effort: bool,
}

/// The added models in force, after MODELS.
static ADDED: std::sync::RwLock<Vec<&'static Model>> = std::sync::RwLock::new(Vec::new());

/// Every `Added` made a `Model` this run, so adding one back (or saving the
/// list again) reuses it: they live as long as OAK, as MODELS do.
static MADE: Mutex<Vec<(Added, &'static Model)>> = Mutex::new(Vec::new());

fn made(a: &Added) -> &'static Model {
    let mut all = MADE.lock().unwrap();
    if let Some((_, m)) = all.iter().find(|(x, _)| x == a) {
        return m;
    }
    let leak = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
    let price = a.free.then_some((0.0, 0.0));
    let m: &'static Model = Box::leak(Box::new(Model {
        id: leak(&a.id),
        name: leak(if a.name.is_empty() { &a.id } else { &a.name }),
        max_output: a.max_output.max(1024),
        context: a.context.max(a.max_output).max(8192),
        max_price: price,
        list_price: price,
        efforts: if a.effort && a.reasoning { &["low", "medium", "high"] } else { &[] },
        reasoning: a.reasoning,
    }));
    all.push((a.clone(), m));
    m
}

/// Puts the models added in Settings in the picker (those MODELS has
/// already, or listed twice, once).
pub fn set_added(list: &[Added]) {
    let mut out: Vec<&'static Model> = vec![];
    for a in list {
        if !a.id.is_empty() && MODELS.iter().all(|m| m.id != a.id) && out.iter().all(|m| m.id != a.id) {
            out.push(made(a));
        }
    }
    *ADDED.write().unwrap() = out;
}

/// The models in the picker: MODELS, then those added in Settings.
pub fn models() -> Vec<&'static Model> {
    MODELS.iter().chain(ADDED.read().unwrap().iter().copied()).collect()
}

/// A model of OpenRouter's list, for Settings' picker.
#[derive(Clone, Debug)]
pub struct Listed {
    pub added: Added,
    /// Dollars per million tokens in and out (its cheapest provider's).
    pub price: (f64, f64),
}

/// OpenRouter's list of models, as fetched for Settings' picker.
#[derive(Clone, Default)]
pub struct Catalog {
    pub list: Vec<Listed>,
    pub loading: bool,
    pub error: Option<String>,
    /// When it was fetched, or last tried.
    at: i64,
}

static CATALOG: Mutex<Option<Catalog>> = Mutex::new(None);

/// The models OAK can use from OpenRouter's list (`/models`, which needs no
/// key): those that call tools and write text, by name.
fn catalog_from(v: &Value) -> Vec<Listed> {
    let num = |x: &Value| x.as_f64().or_else(|| x.as_str()?.parse().ok());
    let mut out: Vec<Listed> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let params: Vec<&str> = m["supported_parameters"].as_array()?.iter().filter_map(|p| p.as_str()).collect();
            let text = m["architecture"]["output_modalities"].as_array().is_none_or(|o| o.iter().any(|x| x == "text"));
            if !params.contains(&"tools") || !text {
                return None;
            }
            let price = (num(&m["pricing"]["prompt"])? * 1e6, num(&m["pricing"]["completion"])? * 1e6);
            if price.0 < 0.0 || price.1 < 0.0 {
                // OpenRouter's auto-router: priced per request it routes.
                return None;
            }
            let context = m["context_length"].as_u64().unwrap_or(0);
            let most = m["top_provider"]["max_completion_tokens"].as_u64().unwrap_or(context.min(32_768));
            // "Google: Gemma 4 26B A4B " as the built-in ones are named.
            let name = m["name"].as_str().unwrap_or(id);
            let name = name.split_once(": ").map_or(name, |n| n.1).split_whitespace().collect::<Vec<_>>().join(" ");
            Some(Listed {
                added: Added {
                    id: id.to_string(),
                    name,
                    context,
                    max_output: most.min(131_072).min(context.max(1024)),
                    free: id.ends_with(":free") || price == (0.0, 0.0),
                    reasoning: params.contains(&"reasoning"),
                    effort: params.contains(&"reasoning_effort"),
                },
                price,
            })
        })
        .collect();
    out.sort_by_key(|l| l.added.name.to_lowercase());
    out
}

/// OpenRouter's list for Settings' picker: fetched in the background the
/// first time it is asked for (again after an hour, or half a minute after
/// failing), `ctx` repainted when it comes.
pub fn catalog(ctx: &eframe::egui::Context) -> Catalog {
    let mut c = CATALOG.lock().unwrap();
    let c = c.get_or_insert_with(Catalog::default);
    let now = api::now_ms();
    let wait = if c.error.is_some() { 30_000 } else { 3_600_000 };
    if !c.loading && (c.at == 0 || now - c.at > wait) {
        c.loading = true;
        c.at = now;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = api::http()
                .get(format!("{BASE}/models"))
                .timeout(Duration::from_secs(30))
                .send()
                .map_err(api::network_error)
                .and_then(api::json_of)
                .map(|v| catalog_from(&v));
            let mut c = CATALOG.lock().unwrap();
            let c = c.get_or_insert_with(Catalog::default);
            c.loading = false;
            match r {
                Ok(list) if !list.is_empty() => (c.list, c.error) = (list, None),
                Ok(_) => c.error = Some("OpenRouter listed no models".into()),
                Err(e) => c.error = Some(e),
            }
            ctx.request_repaint();
        });
    }
    c.clone()
}

/// Which providers OpenRouter may pick from for `model` when OAK has no
/// figures to rank them by (`provider_for`). Within these its own routing
/// chooses, per request: the cheapest in effect (counting cache hits) that
/// is fast and reliable right now, staying with one while its prompt cache
/// is warm. Left out: providers dearer than `max_price`, and any that
/// ignore a parameter OAK sends (tools, thinking).
pub fn routing(model: &Model) -> Value {
    let mut p = json!({"quantizations": QUANTS, "require_parameters": true});
    if let Some((input, output)) = model.max_price {
        p["max_price"] = json!({"prompt": input, "completion": output});
    }
    p
}

/// `body` with its price ceiling raised to the model maker's own price,
/// for when no provider under `max_price` can take it; None when it has no
/// lower ceiling to raise.
fn with_list_price(body: &Value) -> Option<Value> {
    let m = model(body["model"].as_str()?)?;
    let (input, output) = m.list_price?;
    let ceiling = json!({"prompt": input, "completion": output});
    if body["provider"]["max_price"].is_null() || body["provider"]["max_price"] == ceiling {
        return None;
    }
    let mut b = body.clone();
    b["provider"]["max_price"] = ceiling;
    Some(b)
}

// Ranking. OpenRouter's own routing judges a provider's price by everyone's
// traffic, mostly short prompts; an agent's requests are long (OAK's DeepSeek
// sessions average ~160k tokens, nearly all read from the cache), so what
// a cache read costs counts about as much as what writing costs, and moving
// a conversation to another provider loses its cache (one miss at 160k costs
// as much as 5–10 cached requests). So OAK ranks the providers itself, from
// OpenRouter's live figures for each endpoint, and names them in `order`:
// OpenRouter tries them in turn, then the rest that pass the filters.

/// Precisions a provider may serve in. 4-bit is in: DeepSeek's own release
/// keeps its experts (most of the model) in fp4, OpenRouter sees no
/// measurable effect of quantization on tool calls, and OAK's test calls
/// through fp4 providers were as good as through fp8 ones.
const QUANTS: &[&str] = &["fp4", "mxfp4", "fp6", "fp8", "fp16", "bf16", "fp32", "unknown"];

/// What ranking assumes of a request beyond its size, from OAK's DeepSeek
/// sessions: about 3k tokens of new (uncached) input and 1.4k written.
const FRESH: f64 = 3_000.0;
const WRITES: f64 = 1_400.0;

/// A conversation stays with its provider while that is at most this much
/// worse than the best: moving costs its cache.
const STAY: f64 = 1.3;

/// How many providers `order` names.
const ORDER: usize = 4;

/// How long endpoint figures are used before they are fetched again.
const ENDPOINTS_FRESH_MS: i64 = 10 * 60_000;

/// One provider endpoint of a model, as OpenRouter lists it.
#[derive(Clone, Debug, PartialEq)]
struct Endpoint {
    /// "morph/fp8": what `order` takes.
    tag: String,
    /// "Morph": what replies name (`provider_of`).
    provider: String,
    /// Dollars per token: input, output, input read from the cache.
    price: (f64, f64, f64),
    context: u64,
    max_out: u64,
    /// The share of requests it answered lately (the worse of the last day
    /// and the last half hour).
    up: f64,
    /// Median seconds to the first token, and tokens per second, over the
    /// last half hour (only sent with a key; None when not measured).
    latency: Option<f64>,
    tps: Option<f64>,
}

/// The endpoints in OpenRouter's list for `m` that may serve OAK: a
/// precision in QUANTS, not down, the standard tier (not "flex", which
/// OpenRouter only uses when asked, or "priority"), tools and reasoning
/// supported, and no dearer than the model maker's own price.
fn endpoints_from(v: &Value, m: &Model) -> Vec<Endpoint> {
    let num = |x: &Value| x.as_f64().or_else(|| x.as_str()?.parse().ok());
    let list = v["data"]["endpoints"].as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter_map(|e| {
            let params: Vec<&str> = e["supported_parameters"].as_array()?.iter().filter_map(|p| p.as_str()).collect();
            let p = &e["pricing"];
            let (input, output) = (num(&p["prompt"])?, num(&p["completion"])?);
            let read = num(&p["input_cache_read"]).unwrap_or(input);
            let ok = QUANTS.contains(&e["quantization"].as_str().unwrap_or("unknown"))
                && e["status"].as_i64().unwrap_or(0) >= 0
                && e["service_tier"].is_null()
                && params.contains(&"tools")
                && (!m.reasoning || params.contains(&"reasoning"))
                && m.list_price.is_none_or(|l| input * 1e6 <= l.0 + 1e-9 && output * 1e6 <= l.1 + 1e-9);
            if !ok {
                return None;
            }
            let up = [&e["uptime_last_1d"], &e["uptime_last_30m"]].iter().filter_map(|u| u.as_f64()).fold(100.0, f64::min) / 100.0;
            Some(Endpoint {
                tag: e["tag"].as_str()?.to_string(),
                provider: e["provider_name"].as_str()?.to_string(),
                price: (input, output, read),
                context: e["context_length"].as_u64().unwrap_or(0),
                max_out: e["max_completion_tokens"].as_u64().unwrap_or(u64::MAX),
                up,
                latency: num(&e["latency_last_30m"]["p50"]).map(|ms| ms / 1000.0),
                tps: num(&e["throughput_last_30m"]["p50"]).filter(|t| *t > 0.0),
            })
        })
        .collect()
}

/// What a request with `tokens` of input costs at `e` with its prompt
/// cached there (dollars).
fn warm_cost(e: &Endpoint, tokens: f64) -> f64 {
    let fresh = FRESH.min(tokens);
    fresh * e.price.0 + (tokens - fresh) * e.price.2 + WRITES * e.price.1
}

/// How OAK picks between providers (Settings, "OpenRouter routing").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routing {
    /// Cost times the square root of time.
    Balanced,
    /// What a request costs, counting cache reads.
    Cheapest,
    /// How long a request takes (first token, then the reply).
    Fastest,
    /// No order named: OpenRouter's own routing within the price ceiling.
    OpenRouter,
}

impl Routing {
    pub const ALL: [Routing; 4] = [Routing::Balanced, Routing::Cheapest, Routing::Fastest, Routing::OpenRouter];

    pub fn id(self) -> &'static str {
        match self {
            Routing::Balanced => "balanced",
            Routing::Cheapest => "cheapest",
            Routing::Fastest => "fastest",
            Routing::OpenRouter => "openrouter",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Routing::Balanced => "Balanced",
            Routing::Cheapest => "Cheapest",
            Routing::Fastest => "Fastest",
            Routing::OpenRouter => "OpenRouter",
        }
    }

    /// An unknown id is Balanced.
    pub fn from_id(id: &str) -> Routing {
        Routing::ALL.into_iter().find(|r| r.id() == id).unwrap_or(Routing::Balanced)
    }
}

/// The picked routing: its place in `Routing::ALL`.
static ROUTING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn set_routing(id: &str) {
    let r = Routing::from_id(id);
    let i = Routing::ALL.iter().position(|x| *x == r).unwrap_or(0);
    ROUTING.store(i, std::sync::atomic::Ordering::Relaxed);
}

fn routing_pick() -> Routing {
    Routing::ALL[ROUTING.load(std::sync::atomic::Ordering::Relaxed).min(Routing::ALL.len() - 1)]
}

/// Each endpoint's score for a request with `tokens` of input, lower is
/// better, by `how`. Balanced: the request's cost times the square root of
/// how long it takes (the first token, then the reply); Cheapest: the cost
/// alone; Fastest: the time alone. Each is raised for providers that fail
/// often (a failure costs a retry: weighted 8×, as OpenRouter weighs them).
/// A figure OpenRouter hasn't measured is taken as the others' median.
fn scores(eps: &[&Endpoint], tokens: f64, how: Routing) -> Vec<f64> {
    let median = |mut v: Vec<f64>, or: f64| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(or)
    };
    let latency = median(eps.iter().filter_map(|e| e.latency).collect(), 1.0);
    let tps = median(eps.iter().filter_map(|e| e.tps).collect(), 100.0);
    eps.iter()
        .map(|e| {
            let secs = e.latency.unwrap_or(latency) + WRITES / e.tps.unwrap_or(tps);
            let base = match how {
                Routing::Cheapest => warm_cost(e, tokens),
                Routing::Fastest => secs,
                Routing::Balanced | Routing::OpenRouter => warm_cost(e, tokens) * secs.sqrt(),
            };
            base * (1.0 + 8.0 * (1.0 - e.up))
        })
        .collect()
}

/// The endpoints to try for a request with `tokens` of input asking for
/// up to `max_out`, best first. The conversation's provider (`current`,
/// the one its last reply came from) goes first while it is within STAY of
/// the best, so its cache stays warm.
fn rank(eps: &[Endpoint], tokens: u64, max_out: u64, current: &str, how: Routing) -> Vec<String> {
    let fits: Vec<&Endpoint> = eps.iter().filter(|e| e.up >= 0.8 && e.max_out >= max_out && e.context >= tokens).collect();
    let s = scores(&fits, tokens as f64, how);
    let mut order: Vec<usize> = (0..fits.len()).collect();
    order.sort_by(|&a, &b| s[a].total_cmp(&s[b]));
    if let (Some(&best), Some(&mine)) = (order.first(), order.iter().find(|&&i| fits[i].provider == current))
        && s[mine] <= STAY * s[best]
    {
        order.retain(|&i| i != mine);
        order.insert(0, mine);
    }
    order.into_iter().take(ORDER).map(|i| fits[i].tag.clone()).collect()
}

/// Endpoint lists by model, with when each was fetched (or last tried).
static ENDPOINTS: Mutex<Vec<(&'static str, i64, Vec<Endpoint>)>> = Mutex::new(Vec::new());

/// `m`'s endpoints, fetched again when the figures are older than
/// ENDPOINTS_FRESH_MS: one request fetches, the others meanwhile use what
/// is known. Empty when they couldn't be fetched yet.
fn endpoints(m: &'static Model) -> Vec<Endpoint> {
    let Ok(key) = KEYS.any_key() else { return vec![] };
    let now = api::now_ms();
    let (stale, known) = {
        let mut all = ENDPOINTS.lock().unwrap();
        let i = all.iter().position(|k| k.0 == m.id).unwrap_or_else(|| {
            all.push((m.id, 0, vec![]));
            all.len() - 1
        });
        let stale = now - all[i].1 > ENDPOINTS_FRESH_MS;
        if stale {
            all[i].1 = now; // this request fetches
        }
        (stale, all[i].2.clone())
    };
    if !stale {
        return known;
    }
    let list = fetch(&format!("/models/{}/endpoints", m.id), &key, 8).map(|v| endpoints_from(&v, m)).unwrap_or_default();
    let mut all = ENDPOINTS.lock().unwrap();
    let Some(k) = all.iter_mut().find(|k| k.0 == m.id) else { return known };
    if list.is_empty() {
        // Tried again in a minute.
        k.1 = now - ENDPOINTS_FRESH_MS + 60_000;
        return known;
    }
    k.2 = list.clone();
    list
}

/// The `provider` object for `body` (to model `m`): OAK's ranking by the
/// picked `Routing` when the endpoints are known, else (or when OpenRouter
/// routing is picked) OpenRouter's own routing within the filters
/// (`routing`).
fn provider_for(m: &'static Model, body: &Value, route: &str) -> Value {
    let how = routing_pick();
    if how == Routing::OpenRouter {
        return routing(m);
    }
    let eps = endpoints(m);
    let tokens = crate::rolling::estimate(body.to_string().len());
    let max_out = body["max_tokens"].as_u64().unwrap_or(m.max_output);
    ranked(m, &rank(&eps, tokens, max_out, route, how))
}

/// The `provider` object naming `order` (falling back past it to the rest
/// that pass the filters, up to the maker's own price), or `routing` when
/// `order` is empty.
fn ranked(m: &Model, order: &[String]) -> Value {
    if order.is_empty() {
        return routing(m);
    }
    let mut p = json!({
        "order": order,
        "allow_fallbacks": true,
        "quantizations": QUANTS,
        "require_parameters": true,
    });
    if let Some((input, output)) = m.list_price {
        p["max_price"] = json!({"prompt": input, "completion": output});
    }
    p
}

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id).or_else(|| ADDED.read().unwrap().iter().copied().find(|m| m.id == id))
}

/// A model in the picker served through OpenRouter.
pub fn serves(model_id: &str) -> bool {
    model(model_id).is_some()
}

/// The models for session titles that Settings picks between (id, name).
/// Nemotron 3.5 Lightning (free) answered every try in a third of a second
/// with good titles; free models allow 1000 requests a day. Gemma 4 31B is
/// the paid one (fractions of a cent a title).
pub const TITLE_CHOICES: &[(&str, &str)] =
    &[("nvidia/nemotron-3.5-lightning:free", "Nemotron 3.5 Lightning"), ("google/gemma-4-31b-it", "Gemma 4 31B")];

/// The picked title model: its place in `TITLE_CHOICES`.
static TITLE_PICK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Picks the title model by id (an unknown one picks the first).
pub fn set_title_model(id: &str) {
    let i = TITLE_CHOICES.iter().position(|c| c.0 == id).unwrap_or(0);
    TITLE_PICK.store(i, std::sync::atomic::Ordering::Relaxed);
}

/// The title models, the picked one first (OpenRouter tries the next when
/// one is busy).
pub fn title_models() -> Vec<&'static str> {
    let pick = TITLE_PICK.load(std::sync::atomic::Ordering::Relaxed).min(TITLE_CHOICES.len() - 1);
    let first = TITLE_CHOICES[pick].0;
    let mut ids = vec![first];
    ids.extend(TITLE_CHOICES.iter().map(|c| c.0).filter(|id| *id != first));
    ids
}

/// One of the OpenRouter models for titles.
pub fn title_model(model_id: &str) -> bool {
    TITLE_CHOICES.iter().any(|c| c.0 == model_id)
}

/// Requests for this model go to OpenRouter: the picker's, and the ones
/// for titles.
pub fn routed(model_id: &str) -> bool {
    serves(model_id) || title_model(model_id)
}

/// What OpenRouter keeps beside a key, for the account box.
#[derive(Clone, Default)]
pub(crate) struct Stats {
    /// How OpenRouter names the key ("sk-or-v1-d8d...fb4"), from /key.
    label: String,
    /// The key's own spending limit left, when it has one (dollars).
    limit_left: Option<f64>,
    /// Credits bought and used, all time (dollars), from /credits.
    credits: Option<(f64, f64)>,
    /// `spent` when the credits were fetched: what was spent since comes
    /// off what is left, until they are fetched again.
    spent_then: f64,
    error: Option<String>,
}

pub(crate) static KEYS: LazyLock<provider::Keys<Stats>> = LazyLock::new(|| {
    provider::Keys::new(provider::KeyCfg {
        name: "OpenRouter",
        kind: api::Provider::OpenRouter,
        file: "openrouter.json",
        env: "OPENROUTER_API_KEY",
        vault: "openrouter",
    })
});

/// An OpenRouter key is set.
pub fn on() -> bool {
    KEYS.on()
}

/// Checks `key` with OpenRouter, then saves it (on the vault when
/// connected to one) beside the others.
pub fn set_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Paste an OpenRouter key.".into());
    }
    let info = fetch("/key", key, 30).map_err(|e| format!("OpenRouter refused the key: {e}"))?;
    KEYS.take_remote(key)?;
    KEYS.edit(&provider::key_id(key), |k| {
        k.extras.error = None;
        read_key(&mut k.extras, &info);
    });
    Ok(())
}

fn fetch(path: &str, key: &str, secs: u64) -> Result<Value, String> {
    let resp = api::http()
        .get(format!("{BASE}{path}"))
        .bearer_auth(key)
        .timeout(Duration::from_secs(secs))
        .send()
        .map_err(api::network_error)?;
    api::json_of(resp)
}

fn read_key(s: &mut Stats, info: &Value) {
    let d = &info["data"];
    s.label = d["label"].as_str().unwrap_or("").to_string();
    s.limit_left = d["limit_remaining"].as_f64();
}

/// Fetches each key's credits left and name, for the account boxes.
pub fn refresh() {
    for k in KEYS.list() {
        let r = fetch("/credits", &k.key, 30).and_then(|c| Ok((c, fetch("/key", &k.key, 30)?)));
        if r.is_ok() {
            KEYS.accepted(&k.id);
        }
        KEYS.edit(&k.id, |k| match r {
            Ok((c, info)) => {
                let d = &c["data"];
                k.extras.credits = Some((d["total_credits"].as_f64().unwrap_or(0.0), d["total_usage"].as_f64().unwrap_or(0.0)));
                k.extras.spent_then = k.spent;
                read_key(&mut k.extras, &info);
                k.extras.error = None;
            }
            Err(e) => k.extras.error = Some(e),
        });
    }
    // For the Usage page's credits chart.
    if let Some(left) = account().and_then(|a| a.left) {
        crate::usage::note_credits(left);
    }
}

/// An OpenRouter key (or all of them) as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    /// How OpenRouter names the key.
    pub label: String,
    /// Dollars left: the credits not used yet (or the key's own limit left,
    /// when that is less), less what was spent since they were fetched.
    pub left: Option<f64>,
    /// Credits bought.
    pub total: Option<f64>,
    /// What OAK spent through it, all told.
    pub spent: f64,
    pub error: Option<String>,
    /// The vault's.
    pub remote: bool,
    /// OpenRouter refused the key itself: requests skip it.
    pub rejected: bool,
}

fn account_of(k: &provider::Key<Stats>) -> Account {
    let s = &k.extras;
    let since = (k.spent - s.spent_then).max(0.0);
    let left = s.credits.map(|(total, used)| {
        let left = total - used;
        s.limit_left.map_or(left, |l| l.min(left)) - since
    });
    let label = if s.label.is_empty() { provider::short_key(&k.key) } else { s.label.clone() };
    let error = match &k.rejected {
        Some(why) => Some(format!("OpenRouter rejected this key ({why}): remove it")),
        None => s.error.clone(),
    };
    Account { id: k.id.clone(), label, left, total: s.credits.map(|c| c.0), spent: k.spent, error, remote: k.remote, rejected: k.rejected.is_some() }
}

/// Each key, in order.
pub fn accounts() -> Vec<Account> {
    KEYS.list().iter().map(account_of).collect()
}

/// The key conversation `conv` is on (`crate::rotation`), else all of
/// them together (`account`).
pub fn account_of_conv(conv: &str) -> Option<Account> {
    let id = crate::rotation::seat(api::Provider::OpenRouter, conv);
    id.and_then(|id| accounts().into_iter().find(|a| a.id == id)).or_else(account)
}

/// All the keys together (what is left and spent summed; the first error,
/// unless another key works), when one is set. Keys of one OpenRouter
/// account count its credits once; with several, the label says how many.
pub fn account() -> Option<Account> {
    let all = accounts();
    let mut first = all.first()?.clone();
    if all.len() > 1 {
        first.label = format!("{} keys", all.len());
    }
    let mut seen: Vec<(f64, f64)> = vec![];
    let (mut left, mut total) = (None::<f64>, None::<f64>);
    for (a, k) in all.iter().zip(KEYS.list()) {
        let credits = k.extras.credits;
        if credits.is_some_and(|c| seen.contains(&c)) {
            continue;
        }
        seen.extend(credits);
        if let Some(l) = a.left {
            left = Some(left.unwrap_or(0.0) + l);
        }
        if let Some(t) = a.total {
            total = Some(total.unwrap_or(0.0) + t);
        }
    }
    let spent = all.iter().map(|a| a.spent).sum();
    let error = if all.iter().any(|a| a.error.is_none()) { None } else { all.iter().find_map(|a| a.error.clone()) };
    Some(Account { left, total, spent, error, remote: all.iter().any(|a| a.remote), rejected: all.iter().all(|a| a.rejected), ..first })
}

/// What a reply cost, in dollars: on its `message_delta` event.
fn cost_of(ev: &Value) -> Option<f64> {
    if ev["type"] != "message_delta" {
        return None;
    }
    ev["usage"]["cost"].as_f64().filter(|c| c.is_finite() && *c >= 0.0)
}

/// "$1.87", "$0.042", "$0.0003": dollars, with enough digits that a few
/// cheap replies show.
pub fn dollars(v: f64) -> String {
    let a = v.abs();
    let digits = if a >= 1.0 || a == 0.0 { 2 } else if a >= 0.01 { 3 } else { 4 };
    format!("{}${a:.digits$}", if v < 0.0 { "-" } else { "" })
}

/// An Oak request as OpenRouter's Messages endpoint takes it: which
/// providers may serve it (`provider_for`, staying with `req.route` while it
/// is a good pick), and the models to fall back on.
fn body_for(req: &Request) -> Value {
    let mut body = crate::wire_anthropic::body(req, "openrouter", false);
    if let Some(m) = model(&req.model) {
        if !m.reasoning
            && let Some(b) = body.as_object_mut()
        {
            b.remove("thinking");
            b.remove("output_config");
        }
        body["provider"] = provider_for(m, &body, req.route.as_deref().unwrap_or(""));
    }
    if !req.fallbacks.is_empty() {
        body["models"] = json!(req.fallbacks);
    }
    body
}

// Sail Research constrains a tool call's arguments to its schema's
// properties in the order they are listed, letting the model skip optional
// ones: once it has written a field, those listed before it can't follow.
// OAK's schemas go out with their properties in alphabetical order (serde_json
// keeps objects sorted), so through Sail LOAD's `input` (before `name`),
// READ's `limit` and `offset` (before `path`), SCREENSHOT's `height`, ASK's
// `options` and more were dropped, the model meaning to write them. Sail is
// sent schemas whose every property is required, the optional ones nullable
// (`sail_schema`): each then has its turn, and the model writes null for
// those it doesn't use (`tools::repair` takes them out). Other providers get
// OAK's schemas as they are (some, Fireworks among them, garble calls under
// the rewritten ones), so the two never meet: a request ranked to Sail goes
// to Sail alone, and any other request keeps Sail out (`split_sail`).

/// OpenRouter's slug for Sail Research, the start of its endpoints' tags.
const SAIL: &str = "sail-research";

fn is_sail(tag: &str) -> bool {
    tag == SAIL || tag.strip_prefix(SAIL).is_some_and(|rest| rest.starts_with('/'))
}

/// `schema` with every property of each object in it required, those that
/// were optional taking null as well (in their type, and enum if any).
fn sail_schema(schema: &mut Value) {
    if let Some(items) = schema.get_mut("items") {
        sail_schema(items);
    }
    let required: Vec<String> = schema["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else { return };
    for (k, p) in props.iter_mut() {
        sail_schema(p);
        if required.contains(k) {
            continue;
        }
        match p.get_mut("type") {
            Some(Value::String(t)) => {
                let t = std::mem::take(t);
                p["type"] = json!([t, "null"]);
            }
            Some(Value::Array(ts)) if !ts.contains(&Value::Null) && !ts.contains(&json!("null")) => ts.push(json!("null")),
            _ => {}
        }
        if let Some(e) = p.get_mut("enum").and_then(Value::as_array_mut).filter(|e| !e.contains(&Value::Null)) {
            e.push(Value::Null);
        }
    }
    let all: Vec<String> = props.keys().cloned().collect();
    schema["required"] = json!(all);
}

/// `body` split by Sail Research (above): Some request for Sail alone, with
/// its schemas, when OAK's ranking puts a Sail endpoint first (as it does
/// while the conversation stays there); and the request for the others,
/// which keeps Sail out: what goes when there is no Sail request, or Sail
/// can't take it.
fn split_sail(mut body: Value) -> (Option<Value>, Value) {
    let order: Vec<String> = body["provider"]["order"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
    let sail = order.first().is_some_and(|t| is_sail(t)).then(|| {
        let mut s = body.clone();
        s["provider"]["order"] = json!(order.iter().filter(|t| is_sail(t)).collect::<Vec<_>>());
        s["provider"]["allow_fallbacks"] = json!(false);
        for t in s["tools"].as_array_mut().into_iter().flatten() {
            sail_schema(&mut t["input_schema"]);
        }
        s
    });
    if body["provider"].is_object() {
        let rest: Vec<&String> = order.iter().filter(|t| !is_sail(t)).collect();
        if rest.is_empty() {
            if let Some(p) = body["provider"].as_object_mut() {
                // OpenRouter's routing within the filters picks among the rest.
                p.remove("order");
                p.remove("allow_fallbacks");
            }
        } else {
            body["provider"]["order"] = json!(rest);
        }
        body["provider"]["ignore"] = json!([SAIL]);
    }
    (sail, body)
}

/// Every request body `stream` sent (the live tests replay them).
#[cfg(test)]
pub(crate) static SENT: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// Which provider OpenRouter sent a reply to: on its `message_start` event.
fn provider_of(ev: &Value) -> Option<&str> {
    if ev["type"] != "message_start" {
        return None;
    }
    ev["message"]["provider"].as_str().filter(|p| !p.is_empty())
}

/// Streams one Oak request through OpenRouter, sending its events to `tx`:
/// failed requests are retried (with `Retry` events), and the cost goes on
/// the total.
pub fn stream(req: Request, tx: Sender<Sse>) {
    let kind = req.purpose.id().to_string();
    let model_id = req.model.clone();
    let (sail, body) = split_sail(body_for(&req));
    #[cfg(test)]
    SENT.lock().unwrap().push(body.clone());
    let conv = req.seat_conv();
    // Sail once, without retries: if it can't take the request, the others
    // can at once.
    let mut resp = match &sail {
        Some(s) => match open(s, &conv, &tx, api::RETRIES) {
            Err(Some(_)) => open(&body, &conv, &tx, 0),
            r => r,
        },
        None => open(&body, &conv, &tx, 0),
    };
    // No provider under the ceiling right now (down, or busy): once more
    // through the dearer ones rather than failing.
    if let Err(Some(Sse::Err(e))) = &resp
        && e.contains("No endpoints found")
        && let Some(wider) = with_list_price(&body)
    {
        resp = open(&wider, &conv, &tx, 0);
    }
    let (resp, key_id) = match resp {
        Ok(r) => r,
        Err(Some(s)) => return drop(tx.send(s)),
        Err(None) => return,
    };
    let mut cost = 0.0;
    let mut meter = crate::usage::Meter::default();
    // Beyond the Messages format: what the reply cost, and who served it.
    let extra = |ev: &Value| {
        let mut out = vec![];
        if let Some(c) = cost_of(ev) {
            cost += c;
            out.push(Event::Cost(c));
        }
        if let Some(p) = provider_of(ev) {
            out.push(Event::ServedBy(p.to_string()));
        }
        out
    };
    api::relay(resp, &tx, "openrouter", extra, |ev| meter.see(ev));
    if cost > 0.0 {
        KEYS.spend(&key_id, cost);
    }
    if meter.seen {
        meter.usd = meter.usd.max(cost);
        crate::usage::record_openrouter(&model_id, kind, &meter);
    }
}

/// What a key has left, for rotation: its credits (dollars), or plenty
/// while they aren't known.
fn left_of(k: &provider::Key<Stats>) -> f64 {
    account_of(k).left.unwrap_or(1e6)
}

/// Sends `body` through conversation `conv`'s key until a response comes
/// that can be read as a stream; a key out of credits passes the request
/// to another. `tries` counts as already made (api::RETRIES: none again).
/// Ok has the key's id; Err carries what the receiver gets, or None when it
/// is gone.
fn open(body: &Value, conv: &str, tx: &Sender<Sse>, tries: u32) -> Result<(reqwest::blocking::Response, String), Option<Sse>> {
    let mut k = KEYS.pick(conv, left_of).map_err(|e| Some(Sse::Err(e)))?;
    let mut tries = tries;
    loop {
        let key = k.key.clone();
        let build = || {
            api::http()
                .post(format!("{BASE}/messages"))
                .bearer_auth(&key)
                .header("anthropic-version", "2023-06-01")
                .header("Accept", "application/json")
                .header("X-Title", "OAK")
                .json(body)
        };
        let model = body["model"].as_str().unwrap_or("");
        let result = send(tx, &mut tries, build, model).map_err(|()| None)?;
        return match result {
            Ok(r) if r.status().is_success() => {
                if !provider::say_headers(tx, r.status().as_u16()) {
                    return Err(None);
                }
                Ok((r, k.id))
            }
            Ok(r) if matches!(r.status().as_u16(), 401..=403) => {
                // Out of credits (402): another key, of another account,
                // may have some. Refused (401, 403: revoked, deleted):
                // another key takes the conversation, and this one waits
                // on Home.
                let credits = r.status() == 402;
                let label = provider::short_key(&k.key);
                let why = api::error_text(r);
                let set_aside = if credits { KEYS.limited(&k.id, api::now_ms() + 3_600_000) } else { KEYS.reject(&k.id, &why) };
                if set_aside
                    && let Ok(next) = KEYS.pick(conv, left_of)
                    && next.id != k.id
                {
                    k = next;
                    if tx.send(Sse::Rotated(account_of(&k).label)).is_err() {
                        return Err(None);
                    }
                    tries = 0;
                    continue;
                }
                Err(Some(Sse::Err(if credits {
                    format!("OpenRouter is out of credits: {why}")
                } else {
                    format!("OpenRouter rejected key {label} ({why}): remove it on Home, or add one that works.")
                })))
            }
            Ok(r) => {
                let status = r.status().as_u16();
                Err(Some(Sse::Err(format!("OpenRouter {}", error_of(status, &r.text().unwrap_or_default(), model).0))))
            }
            Err(e) => Err(Some(Sse::Err(e))),
        };
    }
}

// Rate limits. A free model is served from its provider's free quota,
// which every OpenRouter user shares: OpenRouter answers 429 "temporarily
// rate-limited upstream" (with retry-after: 1) to a share of requests all
// day, as it does when a paid provider is busy. Measured on November 21,
// 2026, one request every 5 s for 75 s: Gemma 4 31B (free) got through 2
// times in 15, Particle-1 (free) 6 in 15, Nemotron 3.5 Super VL (free)
// every time. The refusals come and go request by request, not in long
// spells, so OAK keeps trying every few seconds (`patient_wait`) for up to
// PATIENCE instead of its usual four quick retries, which a one-second
// retry-after used up within five seconds.

/// How long a request rate-limited upstream keeps being retried.
const PATIENCE_MS: i64 = 5 * 60_000;

/// Seconds before patient retry number `n` (from 1): 3, 4, 5... up to 15,
/// or longer when the server asks (`after`).
fn patient_wait(n: u32, after: Option<u64>) -> u64 {
    (2 + n as u64).min(15).max(after.unwrap_or(0).min(60))
}

/// Seconds until OpenRouter's own rate limit lets requests through again
/// (`x-ratelimit-reset`, milliseconds since the epoch), from `now`.
fn reset_in(h: &reqwest::header::HeaderMap, now: i64) -> Option<u64> {
    let at: i64 = h.get("x-ratelimit-reset")?.to_str().ok()?.trim().parse().ok()?;
    Some(((at - now).max(0) as u64).div_ceil(1000))
}

/// What an error reply from OpenRouter says, for `model`: its message,
/// with the provider and what that answered (its `metadata`); and whether
/// it is a rate limit worth waiting out: a provider's ("rate-limited
/// upstream") or OpenRouter's own per-minute one for free models (not the
/// daily one, which lasts until the next day).
fn error_of(status: u16, body: &str, model: &str) -> (String, bool) {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let e = &v["error"];
    let msg = e["message"].as_str().map(str::to_string).unwrap_or_else(|| body.chars().take(500).collect());
    // (The Messages endpoint keeps it beside `error`; the chat one in it.)
    let meta = if v["metadata"].is_object() { &v["metadata"] } else { &e["metadata"] };
    let who = meta["provider_name"].as_str().unwrap_or("");
    let raw = match &meta["raw"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let upstream = status == 429 && raw.contains("rate-limited upstream");
    let per_minute = status == 429 && msg.contains("per-min");
    let text = if upstream {
        let at = if who.is_empty() { String::new() } else { format!(" at {who}") };
        let shared = if model.ends_with(":free") { " (a free quota all OpenRouter users share)" } else { "" };
        format!("HTTP 429: {model} is rate-limited upstream{at}{shared}")
    } else {
        let raw: String = raw.chars().take(300).collect();
        match (who, raw.as_str()) {
            ("", "") => format!("HTTP {status}: {msg}"),
            (who, "") => format!("HTTP {status}: {msg} ({who})"),
            ("", raw) => format!("HTTP {status}: {msg} ({raw})"),
            (who, raw) => format!("HTTP {status}: {msg} ({who}: {raw})"),
        }
    };
    (text, upstream || per_minute)
}

/// Sends what `build` makes until the answer is worth no retry: network
/// failures and server errors go again within `api::RETRIES` (counted in
/// `tries`), and rate limits worth waiting out (`error_of`) every few
/// seconds for up to PATIENCE_MS besides, unless `tries` starts used up
/// (Sail's single try). The receiver is told why before each wait. An
/// error reply read for that comes back as Err; Err(()) when the receiver
/// is gone.
fn send(tx: &Sender<Sse>, tries: &mut u32, build: impl Fn() -> reqwest::blocking::RequestBuilder, model: &str) -> Result<Result<reqwest::blocking::Response, String>, ()> {
    let patient = *tries < api::RETRIES;
    let start = api::now_ms();
    let (mut attempt, mut waited) = (0, 0);
    loop {
        let result = api::send_fresh(&build);
        attempt += 1;
        let r = match result {
            Ok(r) if provider::retryable(Some(r.status().as_u16())) => r,
            Err(e) if *tries < api::RETRIES => {
                *tries += 1;
                if !provider::wait_between(tx, attempt, api::retry_wait(*tries), Some(e)) {
                    return Err(());
                }
                continue;
            }
            other => return Ok(other),
        };
        let status = r.status().as_u16();
        let after = api::retry_after(r.headers()).or_else(|| reset_in(r.headers(), api::now_ms()));
        let (text, limited) = error_of(status, &r.text().unwrap_or_default(), model);
        let text = format!("OpenRouter {text}");
        let wait = if limited && patient {
            if api::now_ms() - start >= PATIENCE_MS {
                return Ok(Err(format!("{text}; still after {} minutes of retries", PATIENCE_MS / 60_000)));
            }
            waited += 1;
            patient_wait(waited, after)
        } else if *tries < api::RETRIES {
            *tries += 1;
            provider::retry_in(after, *tries)
        } else {
            return Ok(Err(text));
        };
        if !provider::wait_between(tx, attempt, wait, Some(text)) {
            return Err(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_and_dollars() {
        let ev = json!({"type": "message_delta", "usage": {"output_tokens": 39, "cost": 0.00004057}});
        assert_eq!(cost_of(&ev), Some(0.00004057));
        assert_eq!(cost_of(&json!({"type": "message_start", "usage": {"cost": 1.0}})), None);
        assert_eq!(cost_of(&json!({"type": "message_delta", "usage": {}})), None);
        assert_eq!(dollars(1.874497), "$1.87");
        assert_eq!(dollars(0.0423), "$0.042");
        assert_eq!(dollars(0.00012), "$0.0001");
        assert_eq!(dollars(0.0), "$0.00");
        assert_eq!(dollars(-0.5), "-$0.500");
    }

    /// A Messages body without what only Anthropic takes; no Claude Code
    /// identity or billing header is added.
    #[test]
    fn request_shape() {
        let req = Request { model: MODELS[0].id.into(), system: "You are Oak.".into(), route: Some("Morph".into()), thinking: true, effort: Some("high".into()), ..Default::default() };
        let body = body_for(&req);
        assert_eq!(body["system"], "You are Oak.");
        assert!(body.get("thread").is_none() && body.get("context_management").is_none() && body.get("models").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(serves("deepseek/deepseek-v4.1-flash-1017") && !serves("claude-opus-5-5"));
        assert_eq!(api::max_output(MODELS[0].id), 131_072);
        // With no key here, no figures to rank by: OpenRouter's routing,
        // nothing dearer than the ceiling, fp4 allowed but not int4.
        assert!(body["provider"].get("order").is_none());
        let q = body["provider"]["quantizations"].as_array().unwrap();
        assert!(q.contains(&json!("fp8")) && q.contains(&json!("fp4")) && !q.contains(&json!("int4")));
        assert_eq!(body["provider"]["max_price"], json!({"prompt": 0.15, "completion": 0.60}));
        // With no provider under that, once more at the maker's own price.
        let wider = with_list_price(&body).unwrap();
        assert_eq!(wider["provider"]["max_price"], json!({"prompt": 0.30, "completion": 1.20}));
        assert_eq!(wider["provider"]["quantizations"], body["provider"]["quantizations"]);
        assert!(with_list_price(&wider).is_none(), "raised once only");
        assert_eq!(body["provider"]["require_parameters"], true);
        let start = json!({"type": "message_start", "message": {"provider": "InferenceNet"}});
        assert_eq!(provider_of(&start), Some("InferenceNet"));
        assert_eq!(provider_of(&json!({"type": "message_delta"})), None);
    }

    /// OpenRouter's error replies as OAK reads them: an upstream rate limit
    /// (as OpenRouter sent it, November 2026) is waited out and named
    /// plainly; others keep the provider's own words.
    #[test]
    fn errors_read() {
        // The Messages endpoint's (what OAK gets), and the chat endpoint's.
        let upstream = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Provider returned error","error_type":"rate_limit_exceeded"},"request_id":"gen-1","metadata":{"raw":"google/gemma-4-31b-it:free is temporarily rate-limited upstream. Please retry shortly, or add your own provider key to use your own limits: https://openrouter.ai/workspaces/default/byok","provider_name":"Google AI Studio","is_byok":false}}"#;
        let chat = r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"google/gemma-4-31b-it:free is temporarily rate-limited upstream. Please retry shortly.","provider_name":"Google AI Studio","is_byok":false}},"user_id":"u"}"#;
        for body in [upstream, chat] {
            let (text, wait) = error_of(429, body, "google/gemma-4-31b-it:free");
            assert!(wait);
            assert_eq!(text, "HTTP 429: google/gemma-4-31b-it:free is rate-limited upstream at Google AI Studio (a free quota all OpenRouter users share)");
        }
        let (text, wait) = error_of(429, &upstream.replace(":free", ""), "google/gemma-4-31b-it");
        assert!(wait && text.ends_with("upstream at Google AI Studio"), "{text}");
        let daily = r#"{"error":{"message":"Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day","code":429}}"#;
        assert_eq!(error_of(429, daily, "x:free"), ("HTTP 429: Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day".into(), false));
        assert!(error_of(429, r#"{"error":{"message":"Rate limit exceeded: free-models-per-min."}}"#, "x:free").1);
        let bad = r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":{"error":"bad schema"},"provider_name":"Chutes"}}}"#;
        assert_eq!(error_of(400, bad, "m"), (r#"HTTP 400: Provider returned error (Chutes: {"error":"bad schema"})"#.into(), false));
        assert_eq!(error_of(502, "<html>bad gateway</html>", "m").0, "HTTP 502: <html>bad gateway</html>");
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(reset_in(&h, 1_000), None);
        h.insert("x-ratelimit-reset", "1795275060000".parse().unwrap());
        assert_eq!(reset_in(&h, 1_795_275_017_500), Some(43));
        assert_eq!(reset_in(&h, 1_795_275_090_000), Some(0));
        let waits: Vec<u64> = (1..=15).map(|n| patient_wait(n, Some(1))).collect();
        assert_eq!(waits[..4], [3, 4, 5, 6]);
        assert_eq!(waits[14], 15);
        assert_eq!(patient_wait(1, Some(40)), 40);
        assert!(waits.iter().sum::<u64>() < (PATIENCE_MS / 1000) as u64, "a dozen and more tries within the patience");
    }

    /// The built-in models: DeepSeek first, the free ones capped at $0
    /// (no dearer retry), Gemma's effort-less thinking.
    #[test]
    fn built_in_models() {
        let ids: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
        assert_eq!(ids, ["deepseek/deepseek-v4.1-flash-1017", "google/gemma-4-31b-it", "google/gemma-4-31b-it:free", "particle-ai/particle-1:free", "nvidia/nemotron-3.5-super-vl:free"]);
        for id in &ids[2..] {
            let req = Request { model: id.to_string(), thinking: true, ..Default::default() };
            let body = body_for(&req);
            assert_eq!(body["provider"]["max_price"], json!({"prompt": 0.0, "completion": 0.0}), "{id}");
            assert!(with_list_price(&body).is_none(), "{id}");
            assert_eq!(body["thinking"]["type"], "adaptive");
        }
        assert!(crate::agent::efforts("google/gemma-4-31b-it").is_empty());
        assert_eq!(crate::agent::effort_for("google/gemma-4-31b-it", "high"), "");
        assert_eq!(crate::agent::effort_for("particle-ai/particle-1:free", "max"), "high");
        assert_eq!(crate::providers::Kind::of("nvidia/nemotron-3.5-super-vl:free"), crate::providers::Kind::OpenRouter);
    }

    /// Settings' picker: OpenRouter's list read for models that call tools;
    /// an added one serves like a built-in, without a price ceiling unless
    /// free, and without thinking when it has none.
    #[test]
    fn added_models() {
        let list = json!({"data": [
            {"id": "acme/tooly", "name": "Acme: Tooly", "context_length": 200_000, "top_provider": {"max_completion_tokens": 400_000},
             "pricing": {"prompt": "0.0000002", "completion": "0.000001"}, "supported_parameters": ["tools", "max_tokens"],
             "architecture": {"output_modalities": ["text"]}},
            {"id": "acme/thinker:free", "name": "Acme: Thinker (free)", "context_length": 100_000, "top_provider": {"max_completion_tokens": null},
             "pricing": {"prompt": "0", "completion": "0"}, "supported_parameters": ["tools", "reasoning", "reasoning_effort"]},
            {"id": "acme/no-tools", "name": "Acme: No Tools", "context_length": 100_000, "pricing": {"prompt": "0", "completion": "0"}, "supported_parameters": ["reasoning"]},
            {"id": "acme/painter", "name": "Acme: Painter", "context_length": 100_000, "pricing": {"prompt": "0", "completion": "0"}, "supported_parameters": ["tools"],
             "architecture": {"output_modalities": ["image"]}},
            {"id": "openrouter/auto", "name": "Auto Router", "context_length": 100_000, "pricing": {"prompt": "-1", "completion": "-1"}, "supported_parameters": ["tools"]}
        ]});
        let cat = catalog_from(&list);
        let ids: Vec<&str> = cat.iter().map(|l| l.added.id.as_str()).collect();
        assert_eq!(ids, ["acme/thinker:free", "acme/tooly"]);
        let (thinker, tooly) = (&cat[0], &cat[1]);
        assert_eq!((tooly.added.name.as_str(), thinker.added.name.as_str()), ("Tooly", "Thinker (free)"));
        assert!(thinker.added.free && thinker.added.reasoning && thinker.added.effort && thinker.added.max_output == 32_768);
        assert!(!tooly.added.free && !tooly.added.reasoning && tooly.added.max_output == 131_072);
        assert!((tooly.price.0 - 0.2).abs() < 1e-9 && (tooly.price.1 - 1.0).abs() < 1e-9);

        assert!(!serves("acme/tooly"));
        set_added(&[tooly.added.clone(), thinker.added.clone(), tooly.added.clone(), Added { id: MODELS[0].id.into(), ..Default::default() }]);
        let all: Vec<&str> = models().iter().map(|m| m.id).collect();
        assert_eq!(all[MODELS.len()..], ["acme/tooly", "acme/thinker:free"], "no doubles, built-ins once");
        assert!(std::ptr::eq(model("acme/tooly").unwrap(), made(&tooly.added)), "made once");
        assert_eq!(crate::agent::efforts("acme/thinker:free"), ["low", "medium", "high"]);
        assert!(crate::agent::efforts("acme/tooly").is_empty());
        let body = body_for(&Request { model: "acme/tooly".into(), thinking: true, effort: Some("high".into()), ..Default::default() });
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none());
        assert!(body["provider"].get("max_price").is_none() && with_list_price(&body).is_none());
        let body = body_for(&Request { model: "acme/thinker:free".into(), thinking: true, ..Default::default() });
        assert_eq!(body["provider"]["max_price"], json!({"prompt": 0.0, "completion": 0.0}));
        assert_eq!(body["thinking"]["type"], "adaptive");
        // Endpoints: no price filter without a ceiling; reasoning only
        // asked of a model that thinks.
        let mut fx = fixture();
        fx["data"]["endpoints"][0]["supported_parameters"] = json!(["tools"]);
        let tooly_m = model("acme/tooly").unwrap();
        let tags: Vec<String> = endpoints_from(&fx, tooly_m).into_iter().map(|e| e.tag).collect();
        assert!(tags.contains(&"wafer/us".to_string()) && tags.contains(&"alibaba".to_string()), "{tags:?}");
        assert!(!endpoints_from(&fx, &MODELS[0]).iter().any(|e| e.tag == "wafer/us"));
        set_added(&[]);
        assert!(!serves("acme/tooly") && models().len() == MODELS.len());
    }

    /// A request ranked to Sail Research goes to Sail alone with schemas
    /// whose every field is required (the optional ones nullable), then to
    /// the others without Sail; any other request keeps Sail out.
    #[test]
    fn sail_research_split() {
        assert!(is_sail("sail-research/fp4") && is_sail("sail-research/fp4/us") && is_sail("sail-research"));
        assert!(!is_sail("sail-research-two/fp8") && !is_sail("morph/fp8"));
        let m = &MODELS[0];
        let tools = crate::tools::definitions_with_kits(false, false, false, &["AGENT".into(), "BROWSER".into(), "GOAL".into()]);
        let body = |order: &[&str]| {
            let order: Vec<String> = order.iter().map(|s| s.to_string()).collect();
            json!({"model": m.id, "tools": tools, "provider": ranked(m, &order)})
        };

        let (sail, rest) = split_sail(body(&["sail-research/fp4", "morph/fp8", "sail-research/fp4/us", "wafer/us"]));
        let sail = sail.expect("ranked first: a Sail request");
        assert_eq!(sail["provider"]["order"], json!(["sail-research/fp4", "sail-research/fp4/us"]));
        assert_eq!(sail["provider"]["allow_fallbacks"], false);
        assert!(sail["provider"].get("ignore").is_none());
        assert_eq!(rest["provider"]["order"], json!(["morph/fp8", "wafer/us"]));
        assert_eq!((&rest["provider"]["ignore"], &rest["provider"]["allow_fallbacks"]), (&json!([SAIL]), &json!(true)));
        assert_eq!(rest["tools"], json!(tools), "the others get OAK's schemas");
        let tool = |b: &Value, n: &str| b["tools"].as_array().unwrap().iter().find(|t| t["name"] == n).unwrap()["input_schema"].clone();
        let load = tool(&sail, "LOAD");
        assert_eq!(load["required"], json!(["input", "name"]));
        assert_eq!(load["properties"]["input"]["type"], json!(["object", "null"]));
        assert_eq!(load["properties"]["name"], tool(&rest, "LOAD")["properties"]["name"], "required fields as they were");
        let read = tool(&sail, "READ");
        assert_eq!((read["required"].clone(), read["properties"]["offset"]["type"].clone()), (json!(["limit", "offset", "path"]), json!(["integer", "null"])));
        let ask = tool(&sail, "ASK");
        assert_eq!(ask["properties"]["questions"]["items"]["required"], json!(["multiple", "options", "question"]));
        assert_eq!(ask["properties"]["questions"]["items"]["properties"]["options"]["type"], json!(["array", "null"]));
        // Every field of every tool (kits' and a subagent's too) has its
        // turn: required, and nullable unless it was required already.
        let agent = crate::tools::definitions_with_kits(true, false, false, &["AGENT".into(), "WINDOW".into()]);
        for t in tools.iter().chain(&agent) {
            let mut s = t["input_schema"].clone();
            sail_schema(&mut s);
            fn check(before: &Value, after: &Value, tool: &str) {
                if let Some(items) = before.get("items") {
                    check(items, &after["items"], tool);
                }
                let Some(props) = before["properties"].as_object() else { return };
                let required = before["required"].as_array().cloned().unwrap_or_default();
                assert_eq!(after["required"].as_array().unwrap().len(), props.len(), "{tool}");
                for (k, p) in props {
                    check(p, &after["properties"][k], tool);
                    if !required.contains(&json!(k)) {
                        let ty = &after["properties"][k]["type"];
                        assert!(ty.as_array().is_some_and(|a| a.contains(&json!("null"))), "{tool}.{k}: {ty}");
                        if let Some(e) = after["properties"][k]["enum"].as_array() {
                            assert!(e.contains(&Value::Null), "{tool}.{k}");
                        }
                    }
                }
            }
            check(&t["input_schema"], &s, t["name"].as_str().unwrap());
        }

        // Ranked elsewhere first, or by OpenRouter: no Sail request, Sail kept out.
        let (none, rest) = split_sail(body(&["morph/fp8", "sail-research/fp4"]));
        assert!(none.is_none());
        assert_eq!((&rest["provider"]["order"], &rest["provider"]["ignore"]), (&json!(["morph/fp8"]), &json!([SAIL])));
        let (none, rest) = split_sail(json!({"model": m.id, "tools": tools, "provider": routing(m)}));
        assert!(none.is_none() && rest["provider"].get("order").is_none());
        assert_eq!(rest["provider"]["ignore"], json!([SAIL]));
        // Sail alone in the order: the rest by OpenRouter's routing.
        let (sail, rest) = split_sail(body(&["sail-research/fp4"]));
        assert!(sail.is_some() && rest["provider"].get("order").is_none() && rest["provider"].get("allow_fallbacks").is_none());
        assert_eq!(rest["provider"]["ignore"], json!([SAIL]));
    }

    /// Endpoints as OpenRouter lists them (October 2026's figures, trimmed).
    fn fixture() -> Value {
        let e = |tag: &str, name: &str, q: &str, p: (f64, f64, f64), max_out: u64, up: f64, lat: Option<f64>, tps: Option<f64>| {
            json!({"tag": tag, "provider_name": name, "quantization": q, "status": 0, "service_tier": null,
                "pricing": {"prompt": format!("{}", p.0 / 1e6), "completion": format!("{}", p.1 / 1e6), "input_cache_read": format!("{}", p.2 / 1e6)},
                "context_length": 1_048_576, "max_completion_tokens": max_out, "uptime_last_1d": up, "uptime_last_30m": 100,
                "supported_parameters": ["tools", "reasoning", "max_tokens"],
                "latency_last_30m": lat.map(|l| json!({"p50": l})), "throughput_last_30m": tps.map(|t| json!({"p50": t}))})
        };
        let mut list = vec![
            e("wafer/us", "Wafer", "mxfp4", (0.038, 0.50, 0.005), 943_718, 100.0, Some(402.0), Some(157.0)),
            e("morph/fp8", "Morph", "fp8", (0.044, 0.383, 0.004), 943_718, 100.0, Some(801.0), Some(103.0)),
            e("deepinfra/fp8", "DeepInfra", "fp8", (0.098, 0.294, 0.0029), 131_072, 100.0, Some(1060.0), Some(53.0)),
            e("inference-net/fp8", "InferenceNet", "fp8", (0.08, 0.55, 0.008), 393_216, 100.0, Some(461.0), Some(123.0)),
            e("relace", "Relace", "unknown", (0.07, 0.50, 0.008), 943_718, 100.0, Some(1946.0), Some(52.0)),
            e("decart/fp4", "Decart", "fp4", (0.123, 0.492, 0.0025), 262_144, 92.07, Some(745.0), Some(154.0)),
            e("baseten", "BaseTen", "unknown", (0.30, 1.20, 0.007), 262_144, 100.0, Some(430.0), Some(152.0)),
            e("alibaba", "Alibaba", "unknown", (0.33, 1.32, 0.033), 393_216, 100.0, None, None),
            e("cheap/int4", "Cheap", "int4", (0.01, 0.05, 0.001), 943_718, 100.0, None, None),
            e("deepinfra/fp8/flex", "DeepInfra", "fp8", (0.078, 0.235, 0.0024), 131_072, 100.0, Some(3170.0), Some(47.0)),
            e("fireworks", "Fireworks", "unknown", (0.30, 1.20, 0.006), 943_718, 93.0, None, None),
        ];
        list[9]["service_tier"] = "flex".into();
        list[10]["status"] = (-2).into();
        json!({"data": {"endpoints": list}})
    }

    #[test]
    fn providers_ranked() {
        let m = &MODELS[0];
        let eps = endpoints_from(&fixture(), m);
        let tags: Vec<&str> = eps.iter().map(|e| e.tag.as_str()).collect();
        // Out: dearer than list price, int4, the flex tier, a provider marked down.
        assert_eq!(tags, ["wafer/us", "morph/fp8", "deepinfra/fp8", "inference-net/fp8", "relace", "decart/fp4", "baseten"]);
        let per_m = |x: f64| (x * 1e9).round() / 1e3;
        assert_eq!((per_m(eps[1].price.0), per_m(eps[1].price.1), per_m(eps[1].price.2)), (0.044, 0.383, 0.004));
        assert!((eps[5].up - 0.9207).abs() < 1e-9 && eps[0].latency == Some(0.402));
        // Balanced at a 160k-token request: Wafer and Morph close (fast or
        // cheap), then DeepInfra (cheap, slow); InferenceNet (dear cache
        // reads), Relace (slow) and BaseTen (list price) behind; Decart's
        // failures cost it its cheap reads.
        let order = rank(&eps, 160_000, 131_072, "", Routing::Balanced);
        assert_eq!(order[..2].iter().map(String::as_str).collect::<Vec<_>>().iter().filter(|t| ["wafer/us", "morph/fp8"].contains(t)).count(), 2, "{order:?}");
        assert_eq!(order[2], "deepinfra/fp8", "{order:?}");
        assert_eq!(order.len(), ORDER);
        let eps_refs: Vec<&Endpoint> = eps.iter().collect();
        let s = scores(&eps_refs, 160_000.0, Routing::Balanced);
        assert!(s[3] > s[1] * 1.3 && s[4] > s[1] * 1.3 && s[6] > s[1] * 2.0 && s[5] > s[1], "{s:?}");
        // A conversation stays with its provider while it is close...
        assert_eq!(rank(&eps, 160_000, 131_072, "DeepInfra", Routing::Balanced)[0], "deepinfra/fp8");
        assert_eq!(rank(&eps, 160_000, 131_072, "Morph", Routing::Balanced)[0], "morph/fp8");
        // ...not when it is far behind, or gone.
        assert_ne!(rank(&eps, 160_000, 131_072, "BaseTen", Routing::Balanced)[0], "baseten");
        assert_ne!(rank(&eps, 160_000, 131_072, "Relace", Routing::Balanced)[0], "relace");
        assert_eq!(rank(&eps, 160_000, 131_072, "Gone", Routing::Balanced)[0], order[0]);
        // Replies longer than an endpoint writes rule it out.
        assert!(!rank(&eps, 160_000, 300_000, "", Routing::Balanced).contains(&"deepinfra/fp8".to_string()));
        // The request names the ranking; with no figures, OpenRouter's routing.
        let p = ranked(m, &order);
        assert_eq!(p["order"], json!(order));
        assert_eq!(p["allow_fallbacks"], true);
        assert_eq!(p["max_price"], json!({"prompt": 0.30, "completion": 1.20}));
        assert!(p["quantizations"].as_array().unwrap().contains(&json!("mxfp4")));
        assert_eq!(ranked(m, &[]), routing(m));
        assert!(rank(&[], 1000, 1000, "Morph", Routing::Balanced).is_empty());
    }

    /// The other presets: Cheapest by cost (cache reads counted), Fastest
    /// by time; OpenRouter names no order.
    #[test]
    fn routing_presets() {
        let eps = endpoints_from(&fixture(), &MODELS[0]);
        // At 160k, cheap cache reads and output win: DeepInfra, then Morph.
        let cheap = rank(&eps, 160_000, 131_072, "", Routing::Cheapest);
        assert_eq!(cheap[..2], ["deepinfra/fp8", "morph/fp8"], "{cheap:?}");
        // Fastest: Wafer (0.4s, 157 tok/s), then BaseTen (list price, fast).
        let fast = rank(&eps, 160_000, 131_072, "", Routing::Fastest);
        assert_eq!(fast[..2], ["wafer/us", "baseten"], "{fast:?}");
        // The current provider still stays while close.
        assert_eq!(rank(&eps, 160_000, 131_072, "Morph", Routing::Fastest)[0], "wafer/us");
        assert_eq!(rank(&eps, 160_000, 131_072, "Wafer", Routing::Cheapest)[0], "deepinfra/fp8");
        assert_eq!(Routing::from_id("fastest"), Routing::Fastest);
        assert_eq!(Routing::from_id("nonsense"), Routing::Balanced);
        set_routing("openrouter");
        assert_eq!(routing_pick(), Routing::OpenRouter);
        set_routing("balanced");
    }
}
