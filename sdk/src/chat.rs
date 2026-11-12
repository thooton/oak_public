//! A transcript a plugin has and Oak draws (`App::transcript`): the
//! messages in Oak's format (`{"role", "content": [blocks]}`, the blocks as
//! Oak's conversations keep them), the lines between them, and what is
//! going on at its end.

use serde_json::Value;

/// A line shown before message `at`.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub at: usize,
    /// Something failed (shown as an error), not a plain note.
    pub failed: bool,
    pub text: String,
    /// When (ms since 1970), or 0.
    pub time: i64,
}

/// A read-only transcript.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chat {
    pub messages: Vec<Value>,
    pub marks: Vec<Mark>,
    /// Working: the status line shows under the last message.
    pub busy: bool,
    pub status: String,
}

/// A message from an agent, as Oak's transcripts write and show them.
pub fn agent_message(from: &str, final_reply: bool, body: &str) -> String {
    let kind = if final_reply { " type=\"final reply\"" } else { "" };
    format!("{AGENT_TAG} from=\"{from}\"{kind}>\n{}\n</agent-message>", body.trim())
}

/// How an agent message starts.
pub const AGENT_TAG: &str = "<agent-message";
