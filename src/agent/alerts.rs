//! The ACCOUNTS tool: the OpenRouter credit, the only account a model is told
//! about. Claude and Codex plan limits are Oak's business: it rotates
//! accounts and waits for resets by itself. (Oak no longer sends
//! `<usage-alert>`s about the balance; old ones still show in transcripts.)

use super::*;

/// The OpenRouter balance (dollars) under which ACCOUNTS says what $0 means.
pub const LOW_BALANCE: f64 = 0.50;

/// What ACCOUNTS returns: the OpenRouter credit (the only account a model
/// is told about).
pub(crate) fn accounts_text(model: &str) -> String {
    let mut out = format!("You are on {}.\n", api::model_name(model));
    match crate::openrouter::account() {
        Some(a) => {
            let left = a.left.map_or("balance unknown (not fetched yet)".to_string(), |l| format!("{} left", crate::openrouter::dollars(l)));
            let total = a.total.map_or(String::new(), |t| format!(" of {} bought", crate::openrouter::dollars(t)));
            out += &format!("OpenRouter: {left}{total}; {} spent through Oak since it started.\n", crate::openrouter::dollars(a.spent));
            if a.left.is_some_and(|l| l <= LOW_BALANCE) {
                out += "At $0, requests on OpenRouter models fail; credits are added at openrouter.ai.\n";
            }
        }
        None => out += "No OpenRouter key is set.\n",
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_credit_is_reported() {
        let t = accounts_text("local/x");
        assert!(t.starts_with("You are on x (local)."), "{t}");
        assert!(!t.contains("Claude") && !t.contains("Codex") && !t.contains("5-hour") && !t.contains("weekly"), "{t}");
    }
}