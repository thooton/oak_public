//! Models a plugin serves (`oak_sdk::ModelProvider`, the interface's
//! `OakProvider`): the pickers list them under the plugin's provider name
//! (`Kind::Plugin`), conversations name them `plugin:<provider>/<model>`,
//! and their requests go to the plugin whole, as JSON (the system prompt,
//! the messages in Oak's format, the tools). The plugin's events come back
//! here and become Oak's (`oak::Event`); Oak runs the tool calls in them
//! as for any provider. Only normal conversations use them: not chats,
//! Code Mode or subagents, and they get the basic tools (a local model's).

use crate::api::Sse;
use crate::oak::{self, Event, Request};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// What every plugin model's id starts with.
pub const PREFIX: &str = "plugin:";

/// A plugin's model, as Oak lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    /// Oak's id: `plugin:<provider>/<model>`.
    pub id: String,
    pub name: String,
    pub context: u64,
    pub max_output: u64,
    pub efforts: Vec<&'static str>,
    pub images: bool,
}

/// Whether `model` is a plugin's (whether or not its plugin is running).
pub fn serves(model: &str) -> bool {
    model.starts_with(PREFIX)
}

/// The provider and the provider's own model id `model` names.
pub fn split(model: &str) -> Option<(&str, &str)> {
    model.strip_prefix(PREFIX)?.split_once('/')
}

/// Oak's id for provider `provider`'s model `model`.
pub fn id_of(provider: &str, model: &str) -> String {
    format!("{PREFIX}{provider}/{model}")
}

/// A model as a plugin lists it (`ModelInfo`'s JSON).
fn read_model(provider: &str, v: &Value) -> Option<Model> {
    let own = v["id"].as_str().filter(|s| !s.is_empty())?;
    let efforts = v["efforts"].as_array().into_iter().flatten().filter_map(|e| e.as_str()).filter_map(|e| crate::agent::EFFORTS.iter().copied().find(|x| *x == e)).collect();
    Some(Model {
        id: id_of(provider, own),
        name: v["name"].as_str().filter(|s| !s.is_empty()).unwrap_or(own).to_string(),
        context: v["context"].as_u64().filter(|n| *n > 0).unwrap_or(32_000),
        max_output: v["max_output"].as_u64().filter(|n| *n > 0).unwrap_or(8_000),
        efforts,
        images: v["images"].as_bool().unwrap_or(false),
    })
}

/// The plugins' models, asked again at most every two seconds (names,
/// windows and the pickers ask for them every frame).
static CACHE: Mutex<Option<(Instant, Vec<Model>)>> = Mutex::new(None);

/// Every plugin provider's models, in the order they registered.
pub fn models() -> Vec<Model> {
    let mut c = CACHE.lock().unwrap();
    if let Some((at, list)) = c.as_ref()
        && at.elapsed() < Duration::from_secs(2)
    {
        return list.clone();
    }
    let list: Vec<Model> = crate::plugins::parts().providers.iter().flat_map(|p| p.models().iter().filter_map(|m| read_model(&p.id, m)).collect::<Vec<_>>()).collect();
    *c = Some((Instant::now(), list.clone()));
    list
}

/// Model `id`, while its plugin lists it.
pub fn model(id: &str) -> Option<Model> {
    if !serves(id) {
        return None;
    }
    models().into_iter().find(|m| m.id == id)
}

/// A plugin provides models now.
pub fn on() -> bool {
    !crate::plugins::parts().providers.is_empty() && !models().is_empty()
}

/// What the pickers call the plugins' provider: the one provider's name
/// ("Duck.ai"), theirs joined when there are several, "Plugins" with none.
pub fn name() -> &'static str {
    let names: Vec<String> = crate::plugins::parts().providers.iter().map(|p| p.name.clone()).filter(|n| !n.is_empty()).collect();
    if names.is_empty() {
        return "Plugins";
    }
    intern(&names.join(" / "))
}

/// `s`, kept for good (each text once).
fn intern(s: &str) -> &'static str {
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap();
    if let Some(n) = names.iter().find(|n| **n == s) {
        return n;
    }
    let n: &'static str = Box::leak(s.to_string().into_boxed_str());
    names.push(n);
    n
}

/// The first provider's logo (a PNG), if it has one.
pub fn logo() -> Option<Vec<u8>> {
    crate::plugins::parts().providers.iter().find_map(|p| p.logo())
}

/// `req` as a plugin's provider reads it (`oak_sdk::provider::Request`).
fn request_json(req: &Request, own: &str) -> Value {
    json!({
        "model": own,
        "system": req.system,
        "messages": req.messages_with_instruction(),
        "tools": req.tools,
        "tools_off": req.tools_off,
        "effort": req.effort,
        "thinking": req.thinking,
        "max_tokens": req.max_tokens,
        "purpose": req.purpose.id(),
        "conv": req.conv,
        "folder": req.folder,
    })
}

/// The plugin's events as Oak's: blocks opened and closed around them.
#[derive(Default)]
struct Blocks {
    /// The open block: its index and kind ("text", "reasoning").
    open: Option<(usize, &'static str)>,
    next: usize,
    calls: usize,
    stop: Option<String>,
}

impl Blocks {
    fn close(&mut self, out: &mut Vec<Event>) {
        if let Some((i, _)) = self.open.take() {
            out.push(Event::BlockEnd { index: i });
        }
    }

    /// The open block of `kind`, opened (after closing another) if need be.
    fn of(&mut self, kind: &'static str, out: &mut Vec<Event>) -> usize {
        match self.open {
            Some((i, k)) if k == kind => i,
            _ => {
                self.close(out);
                let i = self.next;
                self.next += 1;
                let block = if kind == "text" { oak::text("") } else { json!({"type": "reasoning", "text": "", "by": "plugin"}) };
                out.push(Event::Block { index: i, block });
                self.open = Some((i, kind));
                i
            }
        }
    }

    /// One event of the plugin's: Oak's events for it.
    fn take(&mut self, v: &Value, attempt: &mut u32) -> Vec<Event> {
        let mut out = vec![];
        if let Some(t) = v["text"].as_str().filter(|t| !t.is_empty()) {
            let index = self.of("text", &mut out);
            out.push(Event::Text { index, text: t.to_string() });
        } else if let Some(t) = v["thinking"].as_str().filter(|t| !t.is_empty()) {
            let index = self.of("reasoning", &mut out);
            out.push(Event::Reasoning { index, text: t.to_string() });
        } else if let Some(c) = v.get("tool_call").filter(|c| c["name"].is_string()) {
            self.close(&mut out);
            let index = self.next;
            self.next += 1;
            self.calls += 1;
            let id = c["id"].as_str().filter(|s| !s.is_empty()).map_or_else(|| format!("call_{index}"), String::from);
            out.push(Event::Block { index, block: json!({"type": "tool_call", "id": id, "name": c["name"], "input": {}}) });
            let input = if c["input"].is_object() { c["input"].clone() } else { json!({}) };
            out.push(Event::Input { index, json: input.to_string() });
            out.push(Event::BlockEnd { index });
        } else if let Some(why) = v["retry"].as_str() {
            *attempt += 1;
            out.push(Event::Retry { attempt: *attempt, wait: v["wait"].as_u64().unwrap_or(0), reason: why.to_string() });
        } else if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            out.push(Event::Usage(oak::Usage { input: u["input"].as_u64().unwrap_or(0), output: u["output"].as_u64().unwrap_or(0), ..Default::default() }));
        } else if let Some(s) = v["stop"].as_str() {
            self.stop = Some(s.to_string());
        }
        out
    }

    /// The reply ended: the last block closed, and why it ended.
    fn end(&mut self) -> Vec<Event> {
        let mut out = vec![];
        self.close(&mut out);
        let reason = self.stop.clone().unwrap_or_else(|| if self.calls > 0 { oak::stop::TOOL_CALLS } else { oak::stop::END }.to_string());
        out.push(Event::Stop { reason, detail: None });
        out
    }
}

/// Streams one Oak request through the plugin that serves its model,
/// sending Oak's events to `tx`.
pub fn stream(req: Request, tx: Sender<Sse>) {
    let Some((provider, own)) = split(&req.model) else {
        return drop(tx.send(Sse::Err(format!("{} isn't a plugin's model.", req.model))));
    };
    let Some(p) = crate::plugins::parts().providers.iter().find(|p| p.id == provider).cloned() else {
        return drop(tx.send(Sse::Err(format!("No plugin provides \"{provider}\" now: turn its plugin on (Home, Plugins), or pick another model."))));
    };
    let body = request_json(&req, own);
    if tx.send(Sse::Event(Event::Start { id: String::new(), model: req.model.clone() })).is_err() {
        return;
    }
    let mut blocks = Blocks::default();
    let mut attempt = 0;
    let mut gone = false;
    let result = p.stream(&body, &mut |text| {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return !gone };
        for e in blocks.take(&v, &mut attempt) {
            if tx.send(Sse::Event(e)).is_err() {
                gone = true;
            }
        }
        !gone
    });
    if gone {
        return;
    }
    match result {
        Ok(()) => {
            for e in blocks.end() {
                if tx.send(Sse::Event(e)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Sse::Done);
        }
        Err(e) => drop(tx.send(Sse::Err(if e.is_empty() { format!("{} failed.", p.name) } else { e }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_the_provider_and_its_model() {
        assert_eq!(id_of("duck", "tinfoil/gpt-oss-120b"), "plugin:duck/tinfoil/gpt-oss-120b");
        assert_eq!(split("plugin:duck/tinfoil/gpt-oss-120b"), Some(("duck", "tinfoil/gpt-oss-120b")));
        assert!(serves("plugin:duck/x") && !serves("openai/gpt-5") && split("plugin:duck").is_none());
        let m = read_model("duck", &json!({"id": "gpt-6-luna", "name": "GPT-6 Luna", "efforts": ["low", "medium", "none"], "images": true})).unwrap();
        assert_eq!((m.id.as_str(), m.efforts.clone(), m.images, m.context), ("plugin:duck/gpt-6-luna", vec!["low", "medium"], true, 32_000));
        assert!(read_model("duck", &json!({"name": "no id"})).is_none());
    }

    /// Text, thinking and calls become blocks of their own, each closed
    /// before the next opens; the stop reason follows the calls.
    #[test]
    fn events_become_blocks() {
        let mut b = Blocks::default();
        let mut n = 0;
        let mut all = vec![];
        for v in [json!({"thinking": "hm"}), json!({"text": "Reading "}), json!({"text": "it."}), json!({"tool_call": {"id": "a", "name": "READ", "input": {"path": "x"}}}), json!({"retry": "busy", "wait": 3})] {
            all.extend(b.take(&v, &mut n));
        }
        all.extend(b.end());
        assert_eq!(all[0], Event::Block { index: 0, block: json!({"type": "reasoning", "text": "", "by": "plugin"}) });
        assert_eq!(all[2], Event::BlockEnd { index: 0 });
        assert_eq!(all[3], Event::Block { index: 1, block: oak::text("") });
        assert_eq!(all[5], Event::Text { index: 1, text: "it.".into() });
        assert_eq!(all[6], Event::BlockEnd { index: 1 });
        assert_eq!(all[7], Event::Block { index: 2, block: json!({"type": "tool_call", "id": "a", "name": "READ", "input": {}}) });
        assert_eq!(all[8], Event::Input { index: 2, json: "{\"path\":\"x\"}".into() });
        assert_eq!(all[10], Event::Retry { attempt: 1, wait: 3, reason: "busy".into() });
        assert_eq!(all.last(), Some(&Event::Stop { reason: oak::stop::TOOL_CALLS.into(), detail: None }));
    }
}
