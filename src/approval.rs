//! The user's permission for the model to use something of theirs: one of
//! their own windows (WINDOW action=attach) or one of their browser's tabs
//! (BROWSER action=attach). A tool call asks (`ask`, blocking its thread)
//! and the conversation's transcript shows a card at its end (`view`,
//! drawn by `chat_view.rs`) listing what the model may have, the model's
//! reason and its suggestion picked; the user picks one and allows it, or
//! declines. Every request is asked anew: nothing is remembered.

use crate::theme::Theme;
use eframe::egui::{self, Button, Margin, RichText, Stroke, Ui};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// One thing the user may let the model have.
#[derive(Clone)]
pub struct Choice {
    pub label: String,
    /// A second, dimmer line (a tab's address, a window's program).
    pub detail: String,
}

struct Request {
    id: u64,
    /// What is asked for ("one of your windows").
    what: String,
    /// The model's reason, in its words.
    why: String,
    choices: Vec<Choice>,
    picked: Option<usize>,
    /// Some(Some(i)): allowed choice i; Some(None): declined.
    answer: Option<Option<usize>>,
}

/// Each conversation's request waiting for the user (one at a time).
static PENDING: LazyLock<Mutex<HashMap<String, Request>>> = LazyLock::new(Mutex::default);

/// How long a request waits for the user.
const WAIT: Duration = Duration::from_secs(15 * 60);

fn repaint() {
    if let Some(ctx) = crate::plugins::context() {
        ctx.request_repaint();
    }
}

/// Asks the user to let conversation `conv`'s model use one of `choices`
/// (`what`, for `why`; `suggested` picked first). The choice allowed, or
/// why not.
pub fn ask(conv: &str, what: &str, why: &str, choices: Vec<Choice>, suggested: Option<usize>) -> Result<usize, String> {
    if choices.is_empty() {
        return Err(format!("There is nothing to offer: the user has no {what} open."));
    }
    let id = rand::random::<u64>();
    {
        let mut p = PENDING.lock().unwrap();
        if p.contains_key(conv) {
            return Err("Another request is waiting for the user's answer.".into());
        }
        let picked = suggested.filter(|&i| i < choices.len());
        p.insert(conv.to_string(), Request { id, what: what.to_string(), why: why.trim().to_string(), choices, picked, answer: None });
    }
    repaint();
    let t = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let mut p = PENDING.lock().unwrap();
        let Some(r) = p.get(conv).filter(|r| r.id == id) else {
            return Err("The request was withdrawn.".into());
        };
        if let Some(a) = r.answer {
            p.remove(conv);
            drop(p);
            repaint();
            return a.ok_or_else(|| "The user declined.".to_string());
        }
        if t.elapsed() > WAIT {
            p.remove(conv);
            drop(p);
            repaint();
            return Err("The user didn't answer within 15 minutes.".into());
        }
    }
}

/// Withdraws conversation `conv`'s waiting request (its call was abandoned).
pub fn cancel(conv: &str) {
    if PENDING.lock().unwrap().remove(conv).is_some() {
        repaint();
    }
}

/// Whether conversation `conv` has a request waiting.
pub fn waiting(conv: &str) -> bool {
    PENDING.lock().unwrap().contains_key(conv)
}

/// Conversation `conv`'s waiting request, as a card to answer.
pub(crate) fn view(ui: &mut Ui, t: &Theme, conv: &str) {
    let mut p = PENDING.lock().unwrap();
    let Some(r) = p.get_mut(conv) else { return };
    if r.answer.is_some() {
        return;
    }
    ui.add_space(6.0);
    egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.accent)).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Permission").size(11.5).color(t.accent));
        ui.add_space(4.0);
        ui.label(RichText::new(format!("The agent asks to use {}.", r.what)).strong().color(t.ink));
        if !r.why.is_empty() {
            ui.label(RichText::new(format!("\u{201c}{}\u{201d}", r.why)).color(t.ink2));
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(260.0).id_salt(("approval", r.id)).show(ui, |ui| {
            for (i, c) in r.choices.iter().enumerate() {
                let on = r.picked == Some(i);
                let text = if c.detail.is_empty() { c.label.clone() } else { format!("{}  ({})", c.label, c.detail) };
                if ui.radio(on, RichText::new(text).color(if on { t.ink } else { t.ink2 })).clicked() {
                    r.picked = Some(i);
                }
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let ready = r.picked.is_some();
            let allow = ui
                .add_enabled(ready, Button::new(RichText::new("Allow").color(if ready { t.accent } else { t.ink3 })))
                .on_hover_text("Let the agent see and use the one picked, until it lets go or the conversation ends");
            if allow.clicked() {
                r.answer = Some(r.picked);
            }
            if ui.button(RichText::new("Decline").color(t.ink2)).clicked() {
                r.answer = Some(None);
            }
            ui.label(RichText::new("asked anew each time").size(11.5).color(t.ink3));
        });
    });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_answers_a_request() {
        let choices = vec![Choice { label: "a".into(), detail: String::new() }, Choice { label: "b".into(), detail: "x".into() }];
        let c = choices.clone();
        let h = std::thread::spawn(move || ask("appr-test", "a thing", "because", c, Some(1)));
        while !waiting("appr-test") {
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let mut p = PENDING.lock().unwrap();
            let r = p.get_mut("appr-test").unwrap();
            assert_eq!(r.picked, Some(1));
            r.answer = Some(Some(0));
        }
        assert_eq!(h.join().unwrap(), Ok(0));
        let h = std::thread::spawn(move || ask("appr-test", "a thing", "", choices, None));
        while !waiting("appr-test") {
            std::thread::sleep(Duration::from_millis(10));
        }
        PENDING.lock().unwrap().get_mut("appr-test").unwrap().answer = Some(None);
        assert_eq!(h.join().unwrap(), Err("The user declined.".into()));
        assert!(ask("appr-test", "a thing", "", vec![], None).is_err());
    }
}
