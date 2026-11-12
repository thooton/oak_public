//! Rewording a Freebuff agent's prompt: when AGENT_NEW starts an agent on a
//! Freebuff model with a prompt that has one of the trigger words in it,
//! Gemma 4 31B (through OpenRouter) rewrites the prompt to read more like a
//! person wrote it, before the agent gets it. The triggers and the request
//! are in `assets/freebuff-rewrite-triggers.txt` and
//! `assets/freebuff-rewrite-prompt.txt`.

use super::*;

/// The switch: off, no prompt is reworded (Freebuff agents still get
/// theirs as plain messages).
const ENABLED: bool = false;

/// The model that rewrites (OpenRouter's Gemma 4 31B, as Unslop's).
const MODEL: &str = crate::unslop::MODEL;

/// What Gemma is asked; the prompt follows it in `<input>` tags, and the
/// rewrite comes back in `<output>` tags.
const PROMPT: &str = include_str!("../../assets/freebuff-rewrite-prompt.txt");

/// The words that get a prompt rewritten, one per line (`#` lines and
/// blank ones don't count).
const TRIGGERS: &str = include_str!("../../assets/freebuff-rewrite-triggers.txt");

/// The trigger words, lowercased.
fn triggers() -> impl Iterator<Item = String> {
    TRIGGERS.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_lowercase)
}

/// Whether `prompt` has a trigger word in it, in any case, anywhere (also
/// inside a longer word: "delves", "claude-opus").
pub(crate) fn triggered(prompt: &str) -> bool {
    ENABLED && matches(prompt)
}

/// Whether `prompt` has a trigger word in it, switch or not.
fn matches(prompt: &str) -> bool {
    let p = prompt.to_lowercase();
    triggers().any(|t| p.contains(&t))
}

/// The rewrite request for `prompt`.
fn body(prompt: &str) -> crate::oak::Request {
    crate::oak::Request {
        provider: crate::providers::Kind::OpenRouter,
        model: MODEL.into(),
        max_tokens: 16000,
        purpose: crate::oak::Purpose::Rewrite,
        ttl: crate::oak::Ttl::Off,
        messages: vec![json!({"role": "user", "content": [crate::oak::text(&format!("{}\n\n<input>\n{prompt}\n</input>", PROMPT.trim()))]})],
        ..Default::default()
    }
}

/// What is inside `<output>` in Gemma's reply (the whole reply when the
/// tags are missing), trimmed.
fn output(raw: &str) -> &str {
    let body = raw.split_once("<output>").map_or(raw, |(_, b)| b);
    body.split_once("</output>").map_or(body, |(b, _)| b).trim()
}

/// `prompt` rewritten by Gemma, and what that cost through OpenRouter
/// (dollars). Waits for the rewrite. Errs without an OpenRouter key, or
/// when the request fails or comes back empty.
pub(crate) fn rewrite(prompt: &str) -> (Result<String, String>, f64) {
    if !crate::openrouter::on() {
        return (Err("no OpenRouter key".into()), 0.0);
    }
    let events = match rolling::events(&body(prompt)) {
        Ok(e) => e,
        Err(e) => return (Err(e), 0.0),
    };
    let reply = crate::oak::collect(&events);
    let cost = reply.usd;
    let out = output(&reply.text).to_string();
    (if out.is_empty() { Err("the rewrite came back empty".into()) } else { Ok(out) }, cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_in_any_case() {
        assert!(matches("Find the load-bearing parts"));
        assert!(matches("Ask CLAUDE"));
        assert!(matches("It delves into the code"));
        assert!(!matches("Fix the login redirect bug"));
        assert_eq!(triggered("Ask CLAUDE"), ENABLED);
        assert!(triggers().all(|t| !t.starts_with('#')), "comment lines aren't triggers");
    }

    #[test]
    fn output_between_the_tags() {
        assert_eq!(output("Sure.\n<output>\nRead the file.\n</output>"), "Read the file.");
        assert_eq!(output("<output>Read it."), "Read it.");
        assert_eq!(output(" Plain reply \n"), "Plain reply");
    }

    /// One real rewrite through OpenRouter (costs a fraction of a cent):
    /// `cargo test live_rewrite -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_rewrite() {
        assert!(crate::openrouter::KEYS.load(), "needs an OpenRouter key");
        let prompt = "Delve into src/agent.rs and find the load-bearing parts of the agent loop. Claude will review your findings; keep `Worker::run` as it is.";
        let (out, cost) = rewrite(prompt);
        let out = out.unwrap();
        println!("cost ${cost:.5}\n{out}");
        assert!(out.contains("src/agent.rs") && out.contains("Worker::run"), "paths and names kept");
        assert!(!out.contains("<output>") && !out.contains("</output>"));
    }
}
