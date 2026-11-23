//! Mix (`Conversation::mix`): one coding conversation on two models, a
//! planner (a strong, costly model) and an executor (a faster, cheaper
//! one), that hand it to each other with SWITCH, as Google Antigravity's
//! Orchestrator pairs its models. Nothing forks and no subagent starts:
//! there is one transcript, and each request goes to the model the
//! conversation is on now, which reads all of it. It starts on the planner,
//! which reads the code and writes a plan, then switches; the executor
//! carries it out and switches back when it is stuck, at most
//! `Mix::max_help` times per message from the user (0: no limit).
//!
//! A switch is the session box's (`agent::apply_command`: cache cold, the
//! model not told of it), with each model's own effort, and SWITCH's result
//! tells the new model its role, which model it is, and the note it was
//! handed: the one place a model hears of a switch, since a Mix's models
//! work by taking turns. The roles themselves are in the system prompt (`mix/prompt.md`,
//! after the role's), pinned as any system prompt is.

use crate::store::Mix;
use serde_json::{Value, json};

/// Mix's part of the system prompt.
const PROMPT: &str = include_str!("mix/prompt.md");

/// The tool's name.
pub const TOOL: &str = "SWITCH";

/// Mix's part of the system prompt, after the role's.
pub fn prompt() -> &'static str {
    PROMPT
}

/// SWITCH, offered only in a Mix.
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL,
        "description": "Hand this conversation to the other model of the Mix: from the planner to the executor, or back (see Mix in the system prompt). The other model reads the same conversation, this call and its result included, and carries on from here; you stop at the end of this reply. Runs on its own, after the calls before it in the reply.",
        "input_schema": {
            "type": "object",
            "properties": {
                "note": {"type": "string", "description": "What you hand over. As the planner: where to start, or what to do next. As the executor: what you tried, what happened, and what you think is wrong."}
            },
            "required": ["note"],
        },
        "eager_input_streaming": true,
    })
}

/// What a switch does.
#[derive(Debug)]
pub struct Switch {
    pub provider: crate::providers::Kind,
    pub model: String,
    /// The effort the model takes (`agent::effort_for`).
    pub effort: String,
    /// SWITCH's result, for the model it goes to.
    pub result: String,
    /// The transcript line (after "switched to").
    pub mark: String,
    /// The status line until the next request.
    pub status: String,
}

/// Hands `mix` to its other model, with the `note` the model on it now
/// wrote; `effort` is the conversation's now, kept as that model's. Fails,
/// changing nothing, when the executor has called for help as often as its
/// limit allows since the user's last message.
pub fn switch(mix: &mut Mix, note: &str, effort: &str) -> Result<Switch, String> {
    let to_planner = mix.executing;
    if to_planner && mix.max_help > 0 && mix.helps >= mix.max_help {
        return Err(format!(
            "Not switched: you have called the planner for help {} since the user's last message, as often as this conversation allows. Carry on yourself, or end your turn and tell the user what is blocking you.",
            times(mix.helps)
        ));
    }
    // The effort set now stays with the model it was set for.
    if !effort.is_empty() {
        mix.current_mut().effort = effort.to_string();
    }
    mix.executing = !mix.executing;
    if to_planner {
        mix.helps += 1;
    }
    let to = mix.current().clone();
    let name = crate::api::model_name(&to.model);
    let note = note.trim();
    let from = if to_planner { "executor" } else { "planner" };
    let handed = if note.is_empty() {
        String::new()
    } else {
        format!("\n\nNote from the {from}: {note}")
    };
    let result = if to_planner {
        format!(
            "You are now the planner ({name}). The executor needs help.{handed}\n\nWork out what is wrong: read what you need, then fix the plan, or the problem itself if the fix is small. Then call SWITCH with what the executor should do next. Leave the bulk of the work to the executor."
        )
    } else {
        let left = match mix.max_help.saturating_sub(mix.helps) {
            _ if mix.max_help == 0 => String::new(),
            0 => " You can't call for help again before the user's next message.".to_string(),
            n => format!(
                " You can do so {} more before the user's next message.",
                times(n)
            ),
        };
        format!(
            "You are now the executor ({name}). Carry out the plan above with your tools.{handed}\n\nCall SWITCH to hand back to the planner only when you are stuck after a real attempt, or the plan turns out to be wrong.{left}"
        )
    };
    let role = if to_planner { "planner" } else { "executor" };
    Ok(Switch {
        provider: to.provider,
        mark: format!("the {role}, {name} on {}", to.provider.name()),
        effort: crate::agent::effort_for(&to.model, &to.effort).to_string(),
        model: to.model,
        result,
        status: format!("Switching to the {role}"),
    })
}

/// "once", "twice", "3 times".
fn times(n: u32) -> String {
    match n {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MixModel;

    fn mix(max_help: u32) -> Mix {
        Mix {
            planner: MixModel {
                provider: crate::api::Provider::Claude,
                model: "claude-opus-5-5".into(),
                effort: "high".into(),
            },
            executor: MixModel {
                provider: crate::api::Provider::OpenRouter,
                model: "deepseek/deepseek-v4.1-flash".into(),
                effort: "medium".into(),
            },
            executing: false,
            max_help,
            helps: 0,
        }
    }

    #[test]
    fn switches_back_and_forth_within_the_limit() {
        let mut m = mix(1);
        let s = switch(&mut m, "start with store.rs", "max").unwrap();
        assert!(
            m.executing
                && s.model == "deepseek/deepseek-v4.1-flash"
                && s.provider == crate::api::Provider::OpenRouter
        );
        assert!(
            s.result.starts_with("You are now the executor")
                && s.result
                    .contains("Note from the planner: start with store.rs"),
            "{}",
            s.result
        );
        assert!(s.result.contains("once more"), "{}", s.result);
        assert_eq!(
            m.planner.effort, "max",
            "the effort set stays with the model it was set for"
        );
        let s = switch(&mut m, "the build fails", "medium").unwrap();
        assert!(
            !m.executing
                && m.helps == 1
                && s.model == "claude-opus-5-5"
                && s.result.contains("Note from the executor: the build fails")
        );
        let s = switch(&mut m, "", "max").unwrap();
        assert!(
            s.result.contains("can't call for help again") && !s.result.contains("Note from"),
            "{}",
            s.result
        );
        let before = m.clone();
        assert!(
            switch(&mut m, "again", "medium")
                .unwrap_err()
                .contains("once")
        );
        assert_eq!(m, before, "a refused switch changes nothing");
        // The count restarts with the user's next message; 0 is no limit.
        m.helps = 0;
        m.max_help = 0;
        for _ in 0..6 {
            let s = switch(&mut m, "x", "").unwrap();
            assert!(!s.result.contains("more before"));
        }
    }
}
