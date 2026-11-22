//! What a plugin's provider (`plugin::ModelProvider`, Oak 10) is asked and
//! answers: the request for a reply, as Oak hands it over, its models, and
//! the events of a reply. The wire is JSON (`abi::OakProvider`); these are
//! its Rust forms.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A model a provider serves.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    /// The provider's own id for it (Oak names it `plugin:<provider>/<id>`).
    pub id: String,
    /// As the pickers show it.
    pub name: String,
    /// Its context window and the most a reply may write, in tokens.
    pub context: u64,
    pub max_output: u64,
    /// Oak's effort levels it tells apart ("low" to "max"), lowest first;
    /// none: it has no effort setting.
    pub efforts: Vec<String>,
    /// It sees pictures.
    pub images: bool,
}

/// A request for a reply, whole: what the model is told, the conversation
/// so far in Oak's format, and the tools it may call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    /// The provider's own id for the model.
    pub model: String,
    pub system: String,
    /// Oak messages: `{"role": "user" | "assistant", "content": [block]}`,
    /// blocks `text`, `image` (`media_type`, `data` in base64), `document`,
    /// `reasoning`, `tool_call` (`id`, `name`, `input`) and `tool_result`
    /// (`call_id`, `content`, `is_error`).
    pub messages: Vec<Value>,
    /// `{name, description, input_schema}`.
    pub tools: Vec<Value>,
    /// The model may not call tools now (a summary).
    pub tools_off: bool,
    /// One of the model's `efforts`, when it has them.
    pub effort: Option<String>,
    pub thinking: bool,
    pub max_tokens: u64,
    /// "main" (a conversation's reply), "summary", "title", or a plugin's
    /// name for its own requests.
    pub purpose: String,
    /// The conversation it is for (none for a title).
    pub conv: Option<String>,
    /// Its working folder.
    pub folder: Option<String>,
}

impl Request {
    /// A conversation's own reply: the next turn of its chat (not a
    /// summary, a title or another one-off).
    pub fn is_main(&self) -> bool {
        self.purpose == "main"
    }
}

/// The events of a reply (`ModelProvider::stream`'s `event`).
pub mod event {
    use super::*;

    /// More of the reply's text.
    pub fn text(t: &str) -> Value {
        json!({"text": t})
    }

    /// More of its thinking.
    pub fn thinking(t: &str) -> Value {
        json!({"thinking": t})
    }

    /// A whole tool call, for Oak to run.
    pub fn tool_call(id: &str, name: &str, input: Value) -> Value {
        json!({"tool_call": {"id": id, "name": name, "input": input}})
    }

    /// It is tried again in `wait` seconds, because of `why`.
    pub fn retry(why: &str, wait: u64) -> Value {
        json!({"retry": why, "wait": wait})
    }

    pub fn usage(input: u64, output: u64) -> Value {
        json!({"usage": {"input": input, "output": output}})
    }

    /// Why it ended: "end", "tool_calls" or "max_tokens" (by default
    /// "tool_calls" when it called any tool, else "end").
    pub fn stop(reason: &str) -> Value {
        json!({"stop": reason})
    }
}

/// What a failure starts with to have Oak summarize the conversation and
/// ask again (the context or the chat is full).
pub const TOO_LONG: &str = "prompt is too long";
