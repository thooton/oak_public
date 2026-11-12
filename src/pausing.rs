//! Pausing beyond one session: a session's pause and resume take its
//! subagents along (the pause button spins until every one of them has
//! paused, then "all subagents paused" shows for a while), and closing the
//! window with conversations at work asks first: exit now (as an
//! ultrapause) or pause them all and close once they have, the background
//! tasks stopped or left running (`tasks::close_all`). The conversations
//! paused that way are offered for resuming at the next start.

use super::*;

/// How long "all subagents paused" shows, then how long it fades.
const FLASH_SHOW: Duration = Duration::from_secs(15);
const FLASH_FADE: Duration = Duration::from_millis(1500);
/// Exit now waits at most this long for the ultrapaused sessions to record
/// what they stopped, then closes anyway.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// A session's pause that took its subagents along: when all of them had
/// paused (for the "all subagents paused" line).
pub(crate) type GroupPause = Option<Instant>;

/// Closing the window with conversations at work.
pub(crate) enum Closing {
    /// The dialog asks what to do.
    Asking,
    /// Pause all and close: these were paused; the window closes once
    /// they all have.
    Pausing(Vec<String>),
    /// Exit now: these were ultrapaused; the window closes once they have
    /// stopped (or after `EXIT_GRACE`).
    Exiting(Vec<String>, Instant),
    /// Closing: the next close request goes through.
    Done,
}

impl App {
    /// The subagents of `s`, and theirs, that run in this app.
    fn agents_of(&self, s: &Session) -> Vec<&Session> {
        let mut out = vec![];
        let mut todo = s.agent_convs();
        while let Some(id) = todo.pop() {
            if let Some(a) = self.sessions.get(&id) {
                todo.extend(a.agent_convs());
                out.push(a);
            }
        }
        out
    }

    /// Whether any subagent of `s` is still pausing.
    pub(crate) fn agents_pausing(&self, s: &Session) -> bool {
        self.agents_of(s).iter().any(|a| a.pausing())
    }

    /// Pauses session `id` and all its subagents (each one's pause waits for
    /// its request under way, as the button's does).
    pub(crate) fn pause_group(&mut self, id: &str) {
        self.pause_with(
            id,
            |s| {
                s.request_pause();
            },
            Session::pause_on,
        );
    }

    /// Ultrapause: session `id` and all its subagents stop at once.
    pub(crate) fn ultrapause_group(&mut self, id: &str) {
        self.pause_with(id, Session::ultrapause, Session::pause_settled);
    }

    /// Pauses session `id` with `pause`, and so its subagents that aren't
    /// `paused` already; with any, it is a group pause.
    fn pause_with(&mut self, id: &str, pause: impl Fn(&Session), paused: impl Fn(&Session) -> bool) {
        let Some(s) = self.sessions.get(id) else {
            return;
        };
        pause(s);
        let agents = self.agents_of(s);
        for a in agents.iter().filter(|a| !paused(a)) {
            pause(a);
        }
        if !agents.is_empty() {
            self.group_pauses.insert(id.to_string(), None);
        }
    }

    /// Resume: session `id` and all its subagents (a pause still waiting
    /// is cancelled).
    pub(crate) fn resume_group(&mut self, id: &str) {
        self.group_pauses.remove(id);
        let Some(s) = self.sessions.get(id) else { return };
        s.set_paused(false);
        for a in self.agents_of(s) {
            if a.pause_on() {
                a.set_paused(false);
            }
        }
    }

    /// Notes when a group pause has taken hold everywhere; drops the ones
    /// resumed meanwhile.
    pub(crate) fn track_group_pauses(&mut self) {
        for id in self.group_pauses.keys().cloned().collect::<Vec<_>>() {
            let state = self
                .sessions
                .get(&id)
                .map(|s| (s.pause_on(), s.pause_settled() && !self.agents_pausing(s)));
            match state {
                Some((true, settled)) => {
                    let done = self.group_pauses.get_mut(&id).unwrap();
                    if settled && done.is_none() {
                        *done = Some(Instant::now());
                    }
                }
                _ => drop(self.group_pauses.remove(&id)),
            }
        }
    }

    /// How visible "all subagents paused" is for session `id` (0 to 1),
    /// while it shows.
    pub(crate) fn agents_paused_flash(&self, ctx: &egui::Context, id: &str) -> Option<f32> {
        let done = self.group_pauses.get(id).copied().flatten()?;
        let shown = done.elapsed();
        if shown >= FLASH_SHOW + FLASH_FADE {
            return None;
        }
        if shown < FLASH_SHOW {
            ctx.request_repaint_after(FLASH_SHOW - shown);
            return Some(1.0);
        }
        ctx.request_repaint();
        Some(1.0 - (shown - FLASH_SHOW).as_secs_f32() / FLASH_FADE.as_secs_f32())
    }

    /// The conversations at work (`Session::active`): main sessions and
    /// subagents, by id.
    fn active_sessions(&self) -> (Vec<String>, Vec<String>) {
        let (mut main, mut agents) = (vec![], vec![]);
        for (id, s) in &self.sessions {
            // (Idle with background tasks running counts: closing kills them.)
            if s.active() || s.tasks.running() > 0 {
                if s.is_agent() { &mut agents } else { &mut main }.push(id.clone());
            }
        }
        (main, agents)
    }

    /// A close request (the title bar's X, Alt+F4, the taskbar): with
    /// conversations at work, the window stays and the close dialog asks
    /// what to do; otherwise it closes.
    pub(crate) fn intercept_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || matches!(self.closing, Some(Closing::Done)) {
            return;
        }
        let (main, agents) = self.active_sessions();
        if self.closing.is_none() && main.is_empty() && agents.is_empty() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.closing.is_none() {
            self.closing = Some(Closing::Asking);
        }
    }

    /// The close dialog over the dimmed window, and closing once the
    /// conversations have paused or stopped.
    pub(crate) fn close_dialog(&mut self, ctx: &egui::Context, t: &Theme) {
        if self.finish_close(ctx) {
            return;
        }
        let (main, agents) = self.active_sessions();
        let pausing = matches!(self.closing, Some(Closing::Pausing(_)));
        let tasks: usize = main.iter().chain(&agents).map(|id| self.sessions[id].tasks.running()).sum();
        let choice = close_box(ctx, t, active_text(main.len(), agents.len()), tasks, pausing);
        self.close_with(choice, main.into_iter().chain(agents).collect());
    }

    /// Closes once the conversations pausing (or stopping) for the close
    /// have settled, or the exit's grace is up, keeping which were paused
    /// for the next start. True when there's no dialog to show.
    fn finish_close(&mut self, ctx: &egui::Context) -> bool {
        let (ids, deadline) = match &self.closing {
            None | Some(Closing::Done) => return true,
            Some(Closing::Asking) => return false,
            Some(Closing::Pausing(ids)) => (ids, None),
            Some(Closing::Exiting(ids, at)) => (ids, Some(*at)),
        };
        let settled = ids.iter().all(|id| self.sessions.get(id).is_none_or(|s| s.pause_settled() || !s.pause_on()));
        if !settled && !deadline.is_some_and(|at| at.elapsed() >= EXIT_GRACE) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return false;
        }
        self.settings.paused_on_close = ids
            .iter()
            .filter(|id| self.sessions.get(*id).is_some_and(|s| s.pause_on()))
            .cloned()
            .collect();
        store::save_settings(&self.settings);
        self.closing = Some(Closing::Done);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        true
    }

    /// Does what the close dialog's `choice` says to the conversations at
    /// work (`active`).
    fn close_with(&mut self, choice: Option<CloseChoice>, active: Vec<String>) {
        match choice {
            Some(CloseChoice::Pause) => {
                // (Sessions that only have background tasks running have
                // nothing to pause: the tasks stop when Oak closes, or are
                // kept running.)
                let active: Vec<String> = active.into_iter().filter(|id| self.sessions[id].active()).collect();
                for id in &active {
                    self.sessions[id].request_pause();
                }
                self.closing = Some(Closing::Pausing(active));
            }
            Some(CloseChoice::Exit) => {
                // Those already pausing for the close stop too.
                let mut ids = active;
                if let Some(Closing::Pausing(paused)) = &self.closing {
                    ids.extend(paused.iter().filter(|id| !ids.contains(id)).cloned().collect::<Vec<_>>());
                }
                let keep = crate::tasks::keeping_on_close();
                for id in &ids {
                    if let Some(s) = self.sessions.get(id)
                        && !s.pause_settled()
                    {
                        if keep { s.ultrapause_keeping_tasks() } else { s.ultrapause() }
                    }
                }
                self.closing = Some(Closing::Exiting(ids, Instant::now()));
            }
            Some(CloseChoice::Cancel) => {
                // A restart into an update that was asked for is off too,
                // and so is keeping the tasks.
                crate::update::cancel_restart();
                crate::tasks::keep_on_close(false);
                self.closing = None;
            }
            None => {}
        }
    }

    /// At start: the conversations paused when Oak last closed, offered
    /// for resuming (asked once).
    pub(crate) fn take_resume_offer(&mut self) {
        let ids = std::mem::take(&mut self.settings.paused_on_close);
        if ids.is_empty() {
            return;
        }
        store::save_settings(&self.settings);
        self.resume_offer = ids
            .into_iter()
            .filter(|id| self.sessions.get(id).is_some_and(|s| s.conv.lock().unwrap().paused))
            .collect();
    }

    /// The dialog offering to resume the conversations paused on close.
    pub(crate) fn resume_dialog(&mut self, ctx: &egui::Context, t: &Theme) {
        if self.resume_offer.is_empty() {
            return;
        }
        let titles: Vec<String> = self
            .resume_offer
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .map(|s| {
                let c = s.conv.lock().unwrap();
                let title = if c.title.is_empty() { "Untitled".to_string() } else { c.title.clone() };
                if c.parent.is_some() { format!("{title} (subagent)") } else { title }
            })
            .collect();
        let mut choice = None;
        let modal = crate::widgets::dialog("resume-dialog", t).show(ctx, |ui| {
            ui.set_width(380.0);
            let n = titles.len();
            // Oak closed itself and started again (`gpu::recover`).
            let restarted = crate::gpu::restarted();
            let head = match (n, restarted) {
                (1, false) => "Resume the conversation paused on close?".to_string(),
                (n, false) => format!("Resume the {n} conversations paused on close?"),
                (1, true) => "Resume the conversation stopped by the restart?".to_string(),
                (n, true) => format!("Resume the {n} conversations stopped by the restart?"),
            };
            ui.label(RichText::new(head).strong().color(t.ink));
            ui.add_space(8.0);
            if restarted {
                ui.label(RichText::new("Oak restarted because the graphics device was lost (crash.log in Oak's folder says why). Background tasks kept running.").color(t.ink2));
                ui.add_space(8.0);
            }
            for title in &titles {
                ui.add(egui::Label::new(RichText::new(title).color(t.ink2)).truncate());
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui
                    .add(Button::new(RichText::new("Resume").color(t.accent)))
                    .clicked()
                {
                    choice = Some(true);
                }
                if ui.button("Leave paused").clicked() {
                    choice = Some(false);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => {
                for id in std::mem::take(&mut self.resume_offer) {
                    if let Some(s) = self.sessions.get(&id) {
                        s.set_paused(false);
                    }
                }
            }
            Some(false) => self.resume_offer.clear(),
            None => {}
        }
    }
}

/// The close dialog: what's at work (`active`), keeping the background
/// `tasks` running, and the choices (none while `pausing`, but exiting
/// now and cancelling).
fn close_box(ctx: &egui::Context, t: &Theme, active: String, tasks: usize, pausing: bool) -> Option<CloseChoice> {
    let mut choice = None;
    let modal = crate::widgets::dialog("close-dialog", t).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(RichText::new("Close Oak?").strong().color(t.ink));
        ui.add_space(8.0);
        ui.label(RichText::new(active).color(t.ink2));
        // The background tasks: stopped when Oak closes, or left
        // running (the next Oak picks them up again).
        let mut keep = crate::tasks::keeping_on_close();
        if tasks > 0 {
            ui.add_space(8.0);
            let what = if tasks == 1 { "Keep the background task running".to_string() } else { format!("Keep the {tasks} background tasks running") };
            let tip = "They go on after Oak closes, their output to a file. When Oak starts again, their conversations see them in Tasks and are told when they end. Otherwise they are stopped.";
            if ui.add_enabled(!pausing, egui::Checkbox::new(&mut keep, RichText::new(what).color(t.ink2))).on_hover_text(tip).changed() {
                crate::tasks::keep_on_close(keep);
            }
        }
        let tasks_go = if tasks > 0 && keep { " Background tasks keep running." } else if tasks > 0 { " Background tasks are stopped." } else { "" };
        ui.add_space(14.0);
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            if pausing {
                ui.spinner();
                ui.label(RichText::new("Pausing conversations").color(t.ink3));
                ui.add_space(8.0);
            } else if ui
                .add(Button::new(RichText::new("Pause all and close").color(t.accent)))
                .on_hover_text(format!("Each conversation finishes its current request and running commands, then Oak closes.{tasks_go}"))
                .clicked()
            {
                choice = Some(CloseChoice::Pause);
            }
            if ui
                .button("Exit now")
                .on_hover_text(if keep && tasks > 0 {
                    "Ultrapauses every conversation: kills running commands and discards replies in progress. Background tasks keep running."
                } else {
                    "Ultrapauses every conversation: kills running commands and tasks and discards replies in progress."
                })
                .clicked()
            {
                choice = Some(CloseChoice::Exit);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(CloseChoice::Cancel);
            }
        });
    });
    if modal.should_close() && choice.is_none() && !pausing {
        choice = Some(CloseChoice::Cancel);
    }
    choice
}

enum CloseChoice {
    Pause,
    Exit,
    Cancel,
}

/// "There are N currently active conversations (M subagents)."
fn active_text(main: usize, agents: usize) -> String {
    let lead = match main {
        1 => "There is 1 currently active conversation".to_string(),
        n => format!("There are {n} currently active conversations"),
    };
    match agents {
        0 => format!("{lead}."),
        1 => format!("{lead} (1 subagent)."),
        m => format!("{lead} ({m} subagents)."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app_hover_test::{Headless, conv_json};

    /// The whole app's next frame, with a close request when `close`;
    /// returns the texts painted and the root viewport's commands.
    fn frame(h: &mut Headless, app: &mut App, close: bool) -> (Vec<(String, egui::Pos2)>, Vec<egui::ViewportCommand>) {
        let mut input = egui::RawInput::default();
        if close {
            input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
        }
        let out = h.output(app, input);
        let texts = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(x) => Some((x.galley.text().to_string(), x.pos + x.galley.size() / 2.0)),
                _ => None,
            })
            .collect();
        let cmds = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.commands.clone()).unwrap_or_default();
        (texts, cmds)
    }

    fn texts_have(texts: &[(String, egui::Pos2)], t: &str) -> bool {
        texts.iter().any(|x| x.0 == t)
    }

    /// A conversation that is idle but has a background task running still
    /// asks before Oak closes, and the dialog says the task will be stopped.
    #[test]
    #[ignore]
    fn whole_app_close_with_a_task() {
        const ID: &str = "close-test-2";
        let messages = serde_json::json!([{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"text","text":"hello"}]}]);
        let (_dir, mut h, mut app) = Headless::start("close-task", &[conv_json(ID, "Idle one", messages)]);
        h.screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1920.0, 1000.0));
        app.open(ID.into());
        std::thread::sleep(Duration::from_millis(300));
        let (_, cmds) = frame(&mut h, &mut app, true);
        assert!(!cmds.contains(&egui::ViewportCommand::CancelClose), "nothing running: it closes");
        let task = tasks::spawn("sleep 30", std::path::Path::new("/tmp")).unwrap();
        app.sessions[ID].tasks.add(1, task, "wait");
        let (_, cmds) = frame(&mut h, &mut app, true);
        assert!(cmds.contains(&egui::ViewportCommand::CancelClose), "{cmds:?}");
        let mut texts = vec![];
        for _ in 2..6 {
            texts = frame(&mut h, &mut app, false).0;
        }
        assert!(texts_have(&texts, "1 background task is running and will be stopped when Oak closes."), "{texts:?}");
        app.sessions[ID].tasks.kill_all();
    }
}
