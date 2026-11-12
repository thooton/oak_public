//! The usage-limit resets a Claude account is offered (`api::resets`), on
//! Home under the account's card, and the dialog that uses one. Using a
//! reset can't be undone, so the dialog reads the account's resets afresh,
//! says what this one does, when it expires and what is left of the
//! limits it refills, and only uses it once the person has ticked that
//! they understand and then pressed Yes. Nothing else in OAK uses one.

use crate::api::{self, resets};
use crate::theme::Theme;
use crate::widgets::caption;
use eframe::egui::{self, Button, RichText, Ui};
use std::sync::{Arc, Mutex};

/// The resets account `a` holds, each two lines under its card: "↻ Full
/// reset", "expires Dec 10, 3:00 PM · details…". Returns the reset clicked
/// (for its dialog).
pub(crate) fn rows(ui: &mut Ui, t: &Theme, a: &api::Account, busy: bool) -> Option<String> {
    let r = a.resets.as_ref()?;
    let now = api::now_ms();
    let mut picked = None;
    for g in r.held(now) {
        let count = if g.left > 1 { format!(" ×{}", g.left) } else { String::new() };
        let expires = g.expires_at.map(|e| format!("expires {}", crate::time::day_clock(e, now, crate::time::local_offset_ms()))).unwrap_or_else(|| "no expiry given".into());
        let usable = g.usable(r, now);
        let tip = match g.why_not(r, now) {
            Some(why) => format!("Anthropic offers this account a {}: it {}.\n{why}\nClick for details.", g.kind.lower(), g.effect()),
            None => format!("Anthropic offers this account a {}: it {}. Usable now.\nClick for details, and to use it.", g.kind.lower(), g.effect()),
        };
        let row = ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                caption(ui, &format!("↻ {}{count}", g.kind.name()), if usable { t.ink } else { t.ink2 });
                caption(ui, &format!("{expires} · details…"), t.ink3);
            });
        });
        let resp = ui.interact(row.response.rect, egui::Id::unique(("limit-reset-row", a.id, &g.id)), egui::Sense::click());
        if !busy && resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.on_hover_text(&tip).clicked() && !busy {
            picked = Some(g.id.clone());
        }
    }
    picked
}

/// Where the dialog is.
enum Phase {
    /// Reading the account's resets afresh.
    Checking,
    /// Read (or not: why).
    Ready(Result<resets::Resets, String>),
    /// Asking Anthropic to use it.
    Using,
    /// What came of it.
    Done(resets::Outcome),
}

/// The dialog about one reset of one account, asking before using it.
pub(crate) struct ResetDialog {
    account: u64,
    who: String,
    grant: String,
    kind: resets::Kind,
    /// One per dialog: trying again after an unconfirmed attempt can't
    /// use the reset twice.
    request_id: String,
    phase: Arc<Mutex<Phase>>,
    /// The person ticked that they understand.
    understood: bool,
}

impl ResetDialog {
    /// The dialog about reset `grant` of account `a`; it reads the
    /// account's resets afresh first.
    pub(crate) fn open(ctx: &egui::Context, a: &api::Account, grant: &str) -> ResetDialog {
        let kind = a.resets.as_ref().and_then(|r| r.grants.iter().find(|g| g.id == grant)).map_or(resets::Kind::Full, |g| g.kind);
        let who = [&a.email, &a.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("account {}", a.id + 1));
        let d = ResetDialog { account: a.id, who, grant: grant.to_string(), kind, request_id: api::new_uuid(), phase: Arc::new(Mutex::new(Phase::Checking)), understood: false };
        let (phase, ctx, id) = (d.phase.clone(), ctx.clone(), d.account);
        std::thread::spawn(move || {
            let r = resets::fresh(id, 60_000);
            *phase.lock().unwrap() = Phase::Ready(r);
            ctx.request_repaint();
        });
        d
    }

    /// Shows it; false once it is closed (and whether a reset may have
    /// been used, so the account's usage is read again).
    pub(crate) fn show(&mut self, ctx: &egui::Context, t: &Theme) -> (bool, bool) {
        let now = api::now_ms();
        let mut phase = self.phase.lock().unwrap();
        let (mut use_it, mut close) = (false, false);
        let running = matches!(*phase, Phase::Checking | Phase::Using);
        let modal = crate::widgets::dialog("limit-reset", t).show(ctx, |ui| {
            ui.set_width(380.0);
            let text = |ui: &mut Ui, s: &str, c| {
                ui.add(egui::Label::new(RichText::new(s).color(c)).wrap());
            };
            match &*phase {
                Phase::Checking => {
                    ui.label(RichText::new(format!("{} on {}", self.kind.name(), self.who)).size(16.0).color(t.ink));
                    crate::widgets::working(ui, t, "Checking the account's resets with Anthropic…");
                    close = ui.button("Close").clicked();
                }
                Phase::Ready(Err(e)) => {
                    ui.label(RichText::new(format!("{} on {}", self.kind.name(), self.who)).size(16.0).color(t.ink));
                    crate::widgets::error_note(ui, t, &format!("Couldn't check the account's resets, so it can't be used now: {e}"));
                    close = ui.button("Close").clicked();
                }
                Phase::Ready(Ok(r)) => match r.get(&self.grant, now) {
                    None => {
                        ui.label(RichText::new(format!("{} on {}", self.kind.name(), self.who)).size(16.0).color(t.ink));
                        text(ui, "Anthropic no longer offers this reset to the account (used, expired or withdrawn). Nothing was used.", t.ink2);
                        close = ui.button("Close").clicked();
                    }
                    Some(g) => {
                        ui.label(RichText::new(format!("Use the {} on {}?", g.kind.lower(), self.who)).size(16.0).color(t.ink));
                        ui.add_space(4.0);
                        let mut what = format!("It {} now.", g.effect());
                        if let Some(w) = r.weekly_resets_at {
                            what.push_str(&format!(" The weekly limits still reset {}.", resets::when(w, now)));
                        }
                        text(ui, &what, t.ink2);
                        let mut held = if g.total > 1 { format!("{} of {} left; this uses one.", g.left, g.total) } else { "This uses it up.".to_string() };
                        if let Some(e) = g.expires_at {
                            held.push_str(&format!(" Expires {}.", resets::when(e, now)));
                        }
                        text(ui, &held, t.ink2);
                        if let Some(used) = used_now(self.account) {
                            text(ui, &format!("Used now: {used}."), t.ink3);
                        }
                        if !g.label.is_empty() && !g.label.eq_ignore_ascii_case(g.kind.name()) {
                            text(ui, &format!("Anthropic calls it \"{}\".", g.label), t.ink3);
                        }
                        let why = g.why_not(r, now);
                        if let Some(early) = g.early(r).filter(|_| why.is_none()) {
                            text(ui, &early, t.error);
                        }
                        if let Some(why) = &why {
                            crate::widgets::error_note(ui, t, &format!("It can't be used now: {why}"));
                        }
                        ui.add_space(6.0);
                        text(ui, "Using it can't be undone. OAK never uses a reset by itself: not for an agent, nor when it moves a conversation to another account.", t.ink3);
                        let usable = why.is_none();
                        ui.add_enabled(usable, egui::Checkbox::new(&mut self.understood, format!("I understand this uses the {} now", g.kind.lower())));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let yes = Button::new(RichText::new(format!("Yes, use the {}", g.kind.lower())).color(t.error));
                            use_it = ui.add_enabled(usable && self.understood, yes).clicked();
                            close = ui.button("No, keep it").clicked();
                        });
                    }
                },
                Phase::Using => {
                    ui.label(RichText::new(format!("Using the {} on {}", self.kind.lower(), self.who)).size(16.0).color(t.ink));
                    crate::widgets::working(ui, t, "Asking Anthropic…");
                }
                Phase::Done(o) => {
                    ui.label(RichText::new(format!("{} on {}", self.kind.name(), self.who)).size(16.0).color(t.ink));
                    text(ui, &o.text(self.kind), if matches!(o, resets::Outcome::Used { .. }) { t.ink } else { t.ink2 });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        // Unconfirmed: the same request again can't use it twice.
                        if matches!(o, resets::Outcome::Unconfirmed(_)) && ui.button(RichText::new("Try again").color(t.error)).on_hover_text("Asks again with the same request: Anthropic uses the reset at most once").clicked() {
                            use_it = true;
                        }
                        close = ui.button("Close").clicked();
                    });
                }
            }
        });
        // Esc or a click outside closes it, but not while it is using the reset.
        let closed = close || (modal.should_close() && !running && !matches!(*phase, Phase::Using));
        let used = matches!(&*phase, Phase::Done(o) if !o.nothing_used());
        if use_it && !closed {
            *phase = Phase::Using;
            let (p, ctx, id, grant, request) = (self.phase.clone(), ctx.clone(), self.account, self.grant.clone(), self.request_id.clone());
            std::thread::spawn(move || {
                let o = resets::use_reset(id, &grant, &request);
                *p.lock().unwrap() = Phase::Done(o);
                ctx.request_repaint();
            });
        }
        (!closed, used)
    }
}

/// Account `id`'s limits, used: "5h 100% · Week 85%".
fn used_now(id: u64) -> Option<String> {
    let a = api::accounts().into_iter().find(|a| a.id == id)?;
    let parts: Vec<String> = a.limits.iter().map(|l| format!("{} {:.0}%", l.label, l.percent)).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}
