//! Titles: after the first message, the OpenRouter model picked in
//! Settings, or else the newest Haiku (or the session's own model at low
//! effort), names the conversation in the background.

use crate::tools::{Head, one_line};
use super::*;

/// A session title from what a model wrote: one line, single spaces, no
/// quotes or trailing period, at most 80 characters.
pub(crate) fn clean_title(text: &str) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let title = one_line(line);
    let title = title.trim_start_matches('#').trim().trim_matches(['"', '\'', '.', '*', '`']).trim();
    (!title.is_empty()).then(|| title.head(80))
}

/// The newest Haiku's id, looked up once it is known (a failed lookup is
/// tried again next time).
pub(crate) fn haiku() -> Option<String> {
    static ID: Mutex<Option<String>> = Mutex::new(None);
    let mut id = ID.lock().unwrap();
    if id.is_none() {
        *id = api::latest_models().ok()?.into_iter().find(|m| m.id.contains("haiku")).map(|m| m.id);
    }
    id.clone()
}

/// A title of a few words, in sentence case, for a session that starts
/// with `text`, from a side request without thinking. With an OpenRouter
/// key, first to the model picked in Settings there (falling back to
/// `openrouter::title_models`' others); if that fails, to the session's
/// own kind of model: DeepSeek for an OpenRouter session and the session's
/// model for a Codex or NodeClub one (they use no Claude account), the
/// newest Haiku for a Claude one, or `model` itself at low effort if there
/// is no Haiku. Also returns what it cost through OpenRouter (dollars).
pub(crate) fn title_for(text: &str, model: &str) -> (Option<String>, f64) {
    let text: String = text.head(4000);
    let elsewhere = crate::openrouter::serves(model) || crate::codex::serves(model) || crate::nodeclub::serves(model);
    let mut tries: Vec<(String, Option<&str>)> = vec![];
    if crate::openrouter::on() {
        tries.push((crate::openrouter::title_models()[0].to_string(), None));
    }
    // An OpenRouter, Codex or NodeClub session has no Claude account for a Haiku.
    tries.push(match if elsewhere { None } else { haiku() } {
        Some(h) => (h, None),
        None => (model.to_string(), Some("low")),
    });
    let mut usd = 0.0;
    for (m, effort) in tries {
        let Ok(events) = rolling::events(&title_body(&text, &m, effort)) else { continue };
        let reply = crate::oak::collect(&events);
        usd += reply.usd;
        if let Some(t) = clean_title(&reply.text) {
            return (Some(t), usd);
        }
    }
    (None, usd)
}

/// The title request to `model`: no cache (a one-off would only pay for
/// the write) and no thinking (it would spend the 64 tokens on it).
pub(crate) fn title_body(text: &str, model: &str, effort: Option<&str>) -> crate::oak::Request {
    let prompt = format!(
        "Give a short title, 2 to 8 words, for the session that starts with the message below. \
         Write it in sentence case, like a normal sentence: capitalize only the first word and names \
         (of people, products, files, code identifiers, as they are written), not every word \
         (\"Fix the login redirect bug\", not \"Fix The Login Redirect Bug\"). \
         Answer with the title only: no quotes, no trailing period.\n\n<message>\n{text}\n</message>"
    );
    let mut req = crate::oak::Request {
        provider: crate::providers::Kind::of(model),
        model: model.into(),
        max_tokens: 64,
        system: "You name chat sessions for a list of chats.".into(),
        messages: vec![json!({"role": "user", "content": [crate::oak::text(&prompt)]})],
        effort: effort.map(str::to_string),
        purpose: crate::oak::Purpose::Title,
        ttl: crate::oak::Ttl::Off,
        ..Default::default()
    };
    // A free one that fails passes to the next free one.
    if crate::openrouter::title_model(model) {
        let mut ids = crate::openrouter::title_models();
        ids.retain(|id| *id != model);
        ids.insert(0, model);
        req.fallbacks = ids.into_iter().map(String::from).collect();
    }
    req
}
