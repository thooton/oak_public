//! Quick's reply markup, as Oak shows it. Quick's agent writes for Quick's
//! app: a choice it offers is a decision card (`<decision question="...">`
//! with its `<option>`s, several in `<decisions>`), and the memories it used
//! close the reply in `<sources>`. Oak shows neither as text: a card's
//! questions become Oak's ASK (`Turn::close`), whose answers go back as
//! Quick's `<decision_response>` (`decision_response`), and the sources are
//! left out.
//!
//! Only a tag that begins a line counts, outside code fences, so a reply
//! that talks about the markup keeps it.

use serde_json::{Value, json};

/// The tags read out of Quick's text.
const TAGS: [&str; 3] = ["decisions", "decision", "sources"];

/// ASK's limits: questions per ASK, options per question.
const MAX_QUESTIONS: usize = 6;
const MAX_OPTIONS: usize = 10;

/// Quick's text `raw` as Oak shows it, and the questions of its decision
/// cards (ASK's `questions`). While the text still streams (`streaming`), a
/// card or sources not closed yet, or what may be the start of one, is held
/// back until it is; at the end such text is shown as it is.
pub(crate) fn split(raw: &str, streaming: bool) -> (String, Vec<Value>) {
    // Most text has no tags at all (this runs for every delta).
    if !raw.contains('<') {
        return (raw.to_string(), vec![]);
    }
    let mut shown = String::new();
    let mut questions = vec![];
    // A card or sources was left out: the text after it is a paragraph of
    // its own.
    let mut gap = false;
    let mut fence = false;
    let mut at = 0;
    while at < raw.len() {
        let end = raw[at..].find('\n').map_or(raw.len(), |n| at + n + 1);
        let line = &raw[at..end];
        let rest = raw[at..].trim_start_matches([' ', '\t']);
        if rest.starts_with("```") || rest.starts_with("~~~") {
            fence = !fence;
        }
        if !fence && rest.starts_with('<') {
            match open_tag(rest) {
                Tag::Not => {}
                Tag::Partial if streaming => return (shown, questions),
                Tag::Partial => {}
                Tag::Name(name) => {
                    let close = format!("</{name}>");
                    match rest.find(&close) {
                        Some(n) => {
                            let block = &rest[..n + close.len()];
                            if name != "sources" {
                                questions.extend(cards(block));
                            }
                            shown.truncate(shown.trim_end().len());
                            gap = true;
                            let after = &rest[n + close.len()..];
                            at = raw.len() - after.trim_start().len();
                            continue;
                        }
                        None if streaming => return (shown, questions),
                        None => {}
                    }
                }
            }
        }
        if gap {
            if line.trim().is_empty() {
                at = end;
                continue;
            }
            if !shown.is_empty() {
                shown.push_str("\n\n");
            }
            gap = false;
        }
        shown.push_str(line);
        at = end;
    }
    (shown, questions)
}

/// What a `<` that begins a line opens.
enum Tag {
    /// Not one of `TAGS`.
    Not,
    /// The text ends before the tag's name does, which may be one of them.
    Partial,
    Name(&'static str),
}

fn open_tag(s: &str) -> Tag {
    let after = &s[1..];
    let n = after.bytes().take_while(u8::is_ascii_alphabetic).count();
    let name = &after[..n];
    match after[n..].chars().next() {
        None if TAGS.iter().any(|t| t.starts_with(name)) => Tag::Partial,
        None => Tag::Not,
        Some(c) if c == '>' || c.is_whitespace() => match TAGS.iter().copied().find(|t| *t == name) {
            Some(t) => Tag::Name(t),
            None => Tag::Not,
        },
        Some(_) => Tag::Not,
    }
}

/// The questions of the decision cards in `block` (a `<decision>` or a
/// `<decisions>` of them), as ASK takes them.
fn cards(block: &str) -> Vec<Value> {
    let mut out = vec![];
    let mut s = block;
    while let Some(n) = find_tag(s, "decision") {
        let tail = &s[n..];
        let Some(end) = tail.find("</decision>") else {
            break;
        };
        let card = &tail[..end];
        let head_end = tag_end(card).unwrap_or(card.len());
        let body = card.get(head_end + 1..).unwrap_or("");
        if out.len() < MAX_QUESTIONS {
            out.push(question(&card[..head_end], body));
        }
        s = &tail[end + "</decision>".len()..];
    }
    out
}

/// One card's question: its open tag `head` and what it holds, `body`.
fn question(head: &str, body: &str) -> Value {
    let attrs = attributes(head);
    let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let text = get("question")
        .or(get("title"))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Which one?")
        .to_string();
    let multiple = get("multi")
        .or(get("multiple"))
        .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
    let mut options = vec![];
    let mut s = body;
    while let Some(n) = find_tag(s, "option") {
        let tail = &s[n..];
        let Some(head_end) = tag_end(tail) else {
            break;
        };
        let head = &tail[..head_end];
        let after = &tail[head_end + 1..];
        let (label, next) = if head.trim_end().ends_with('/') {
            ("", after)
        } else {
            match after.find("</option>") {
                Some(e) => (&after[..e], &after[e + "</option>".len()..]),
                None => (after, ""),
            }
        };
        s = next;
        let attrs = attributes(head);
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let mut label = words(&decode(&strip_tags(label)));
        if label.is_empty() {
            label = words(&get("label").or(get("value")).unwrap_or_default());
        }
        if label.is_empty() || options.len() == MAX_OPTIONS {
            continue;
        }
        let about = words(&get("description").unwrap_or_default());
        options.push(if about.is_empty() {
            json!(label)
        } else {
            json!({"label": label, "description": about})
        });
    }
    json!({"question": text, "options": options, "multiple": multiple})
}

/// Where tag `name` opens in `s` (not a longer name it begins, as
/// `decision` begins `decisions`).
fn find_tag(s: &str, name: &str) -> Option<usize> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(n) = s[from..].find(&open) {
        let at = from + n;
        match s[at + open.len()..].chars().next() {
            Some(c) if c == '>' || c == '/' || c.is_whitespace() => return Some(at),
            _ => from = at + open.len(),
        }
    }
    None
}

/// Where the tag `s` opens with ends (its `>`, outside quoted values).
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i),
            None => {}
        }
    }
    None
}

/// An open tag's attributes (`head`, without its `>`), names in lower
/// case, values decoded; a bare one (`multi`) has an empty value.
fn attributes(head: &str) -> Vec<(String, String)> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut out = vec![];
    let mut s = head.trim_start_matches('<').trim_start_matches(is_name);
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        let n = s.find(|c: char| !is_name(c)).unwrap_or(s.len());
        if n == 0 {
            break;
        }
        let name = s[..n].to_ascii_lowercase();
        s = s[n..].trim_start();
        let Some(r) = s.strip_prefix('=') else {
            out.push((name, String::new()));
            continue;
        };
        let r = r.trim_start();
        let (value, rest) = match r.chars().next() {
            Some(q @ ('"' | '\'')) => match r[1..].find(q) {
                Some(e) => (&r[1..1 + e], &r[2 + e..]),
                None => (&r[1..], ""),
            },
            _ => {
                let e = r.find(char::is_whitespace).unwrap_or(r.len());
                (&r[..e], &r[e..])
            }
        };
        out.push((name, decode(value)));
        s = rest;
    }
    out
}

/// `s` without the tags in it.
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(n) = rest.find('<') {
        out.push_str(&rest[..n]);
        match tag_end(&rest[n..]) {
            Some(e) => rest = &rest[n + e + 1..],
            None => {
                rest = &rest[n..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `s` with HTML's character references for markup undone.
fn decode(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// `s`'s words, one space between each.
fn words(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// ASK's answers (`1. Question` lines, each with its `Answer: ...`) as
/// Quick's app sends a card's: `<decision_response>` with a `Question: answer`
/// line for each. What isn't answers (the user skipped them) goes as it is.
pub(crate) fn decision_response(answers: &str) -> String {
    let mut lines = vec![];
    let mut question: Option<&str> = None;
    for l in answers.lines() {
        let t = l.trim();
        if let Some(a) = t.strip_prefix("Answer:") {
            if let Some(q) = question.take() {
                lines.push(format!("{q}: {}", a.trim()));
            }
        } else if let Some((n, q)) = t.split_once(". ")
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            question = Some(q.trim());
        }
    }
    if lines.is_empty() {
        return answers.trim().to_string();
    }
    format!("<decision_response>\n{}\n</decision_response>", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "Here are your options:\n<decision question=\"Which direction should I build?\">\n<option description=\"Port Quick&#39;s socket\">Command center workers on Quick</option>\n<option>Local only</option>\n</decision>\n<sources>\n[no tests](mem://e3032ae86dc0)\n</sources>\n";

    #[test]
    fn a_card_is_a_question_and_sources_are_left_out() {
        let (shown, qs) = split(CARD, false);
        assert_eq!(shown, "Here are your options:");
        assert_eq!(
            qs,
            vec![json!({"question": "Which direction should I build?", "options": [
                {"label": "Command center workers on Quick", "description": "Port Quick's socket"},
                "Local only"
            ], "multiple": false})]
        );
    }

    #[test]
    fn grouped_cards_are_several_questions() {
        let raw = "Pick:\n<decisions>\n<decision question=\"Format?\">\n<option>PDF</option>\n</decision>\n<decision question=\"Charts?\" multi=\"true\">\n<option>Bar</option>\n<option>Pie</option>\n</decision>\n</decisions>\nThen I start.";
        let (shown, qs) = split(raw, false);
        assert_eq!(shown, "Pick:\n\nThen I start.");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[1]["question"], "Charts?");
        assert_eq!(qs[1]["multiple"], true);
        assert_eq!(qs[1]["options"], json!(["Bar", "Pie"]));
    }

    #[test]
    fn streaming_holds_back_what_may_be_a_card() {
        assert_eq!(split("Options:\n<deci", true).0, "Options:\n");
        assert_eq!(split("Options:\n<", true).0, "Options:\n");
        assert_eq!(split("Options:\n<decision question=\"A?\">\n<option>x", true).0, "Options:\n");
        assert_eq!(split("Options:\n<b>bold</b>", true).0, "Options:\n<b>bold</b>");
        // At the end, an unclosed card is shown as it is.
        assert_eq!(split("Options:\n<decision q", false).0, "Options:\n<decision q");
        // Every prefix of a reply shows a prefix of what the whole shows, or
        // is held at a card.
        for k in 0..CARD.len() {
            if CARD.is_char_boundary(k) {
                let (part, _) = split(&CARD[..k], true);
                assert!(CARD.starts_with(part.trim_end()) , "{k}: {part:?}");
            }
        }
    }

    #[test]
    fn markup_in_code_or_mid_line_is_text() {
        let raw = "Write it as `<decision>`, or:\n```\n<decision question=\"A?\"><option>x</option></decision>\n```\n";
        let (shown, qs) = split(raw, false);
        assert_eq!(shown, raw);
        assert!(qs.is_empty());
    }

    #[test]
    fn answers_go_back_as_a_decision_response() {
        let a = "1. Which direction should I build?\n   Answer: Local only\n2. Charts?\n   Answer: Bar; Other: a table\n";
        assert_eq!(
            decision_response(a),
            "<decision_response>\nWhich direction should I build?: Local only\nCharts?: Bar; Other: a table\n</decision_response>"
        );
        assert_eq!(
            decision_response("The user skipped these questions without answering."),
            "The user skipped these questions without answering."
        );
    }
}
