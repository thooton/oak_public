//! The session box, top left of a conversation (title, folder, model, effort,
//! goal, progress meters, agents, heartbeats), the model-switch dialog, and the Notes and Tasks
//! boxes under it.

use super::*;
pub(crate) use oak_sdk::ui::{corner_area, corner_width, panel_box};
use crate::widgets::{chevron_v, clip_width, enter_pressed, inset, is_open, left_with, sending_edit, paint_pause, set_open, state_dot};
use crate::theme::mix;
use crate::composer::GAP;
use crate::tools::one_line;

/// What the session box shows: the open conversation, or before one
/// starts, what a new one will use. A goal or effort the user set that
/// hasn't gone out with a request yet shows already.
pub(crate) struct SessionInfo {
    pub(crate) title: String,
    pub(crate) folder: String,
    /// The working folder, when there is one (`folder` reads "no folder
    /// chosen" otherwise): the folder row opens it and copies its path.
    pub(crate) folder_path: Option<std::path::PathBuf>,
    /// The model's name, and its id.
    pub(crate) model: String,
    pub(crate) model_id: String,
    /// Who serves the model.
    pub(crate) provider: api::Provider,
    /// The models it can switch to (the model picker's).
    pub(crate) models: Vec<api::Model>,
    /// As the model takes it (`agent::effort_for`), and its levels.
    pub(crate) effort: String,
    pub(crate) efforts: Vec<&'static str>,
    pub(crate) new: bool,
    pub(crate) goal: String,
    /// The goal's auto-continue is paused.
    pub(crate) goal_paused: bool,
    pub(crate) heartbeats: Vec<store::Heartbeat>,
    pub(crate) meters: Vec<store::Meter>,
    pub(crate) notes: Vec<store::Note>,
    /// The subagents it started: number, name, conversation, state.
    pub(crate) agents: Vec<(u64, String, String, AgentState)>,
    /// For a subagent: its number, and the session that started it
    /// (conversation and title).
    pub(crate) parent: Option<(u64, String, String)>,
    /// Draws the separate agent and system prompt note (cloud sessions).
    pub(crate) note_separate_system: bool,
}

/// What a subagent in the session box is doing.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum AgentState {
    Idle,
    Working,
    /// A pause is under way (a spinner).
    Pausing,
    /// Paused (an empty circle).
    Paused,
}

impl AgentState {
    pub(crate) fn of(s: &Session) -> AgentState {
        if s.pausing() {
            AgentState::Pausing
        } else if s.pause_on() {
            AgentState::Paused
        } else if s.live.lock().unwrap().busy {
            AgentState::Working
        } else {
            AgentState::Idle
        }
    }
}

impl SessionInfo {
    pub(crate) fn new(session: Option<&Session>, settings: &Settings, models: Vec<api::Model>, model_name: impl Fn(&str) -> String) -> SessionInfo {
        // What follows from the model: as a new conversation has it.
        let with = |title: String, folder: String, provider: api::Provider, model: String, effort: &str| SessionInfo {
            title,
            folder_path: (!folder.is_empty() && folder != NO_FOLDER).then(|| std::path::PathBuf::from(&folder)),
            folder,
            model: model_name(&model),
            provider,
            models,
            effort: agent::effort_for(&model, effort),
            efforts: agent::efforts(&model),
            model_id: model,
            new: true,
            goal: String::new(),
            goal_paused: false,
            heartbeats: vec![],
            meters: vec![],
            notes: vec![],
            agents: vec![],
            parent: None,
            note_separate_system: false,
        };
        let Some(s) = session else {
            let folder = settings.folder.trim();
            let folder = if folder.is_empty() { NO_FOLDER.into() } else { folder.to_string() };
            let provider = settings.provider.unwrap_or_else(|| api::Provider::of(&settings.model));
            return with("New conversation".into(), folder, provider, settings.model.clone(), &settings.effort);
        };
        let pending = s.live.lock().unwrap().commands.clone();
        let c = s.conv.lock().unwrap();
        let (mut effort, mut goal, mut model, mut provider) = (c.effort.clone(), c.goal.clone(), c.model.clone(), c.provider());
        for cmd in pending {
            match cmd {
                Command::Goal(g) => goal = g,
                Command::Effort(e) => effort = e,
                Command::Model(p, m) => (provider, model) = (p, m),
            }
        }
        let title = if c.title.is_empty() { "Untitled".into() } else { c.title.clone() };
        SessionInfo {
            new: false,
            // (A new goal waiting to go out starts unpaused.)
            goal_paused: c.goal_paused && !goal.is_empty() && goal == c.goal,
            goal,
            heartbeats: c.heartbeats.clone(),
            meters: c.meters.clone(),
            notes: c.notes.clone(),
            agents: c.agents.iter().map(|a| (a.id, a.name.clone(), a.conv.clone(), AgentState::Idle)).collect(),
            parent: c.parent.clone().map(|p| (c.agent_id, p, String::new())),
            note_separate_system: crate::cloud::is_key(&c.id),
            ..with(title, c.folder.display().to_string(), provider, model, &effort)
        }
    }
}

/// What the folder row reads before a conversation without a folder.
const NO_FOLDER: &str = "no folder chosen";

/// The session box's folder row: a click opens the folder in the system's
/// file manager (Explorer, or the desktop's through xdg-open); the
/// right-click menu opens it or copies its path. Without a folder the row
/// is plain text, and a folder that no longer exists can't be opened.
fn folder_row(ui: &mut Ui, info: &SessionInfo, text: RichText) {
    let Some(path) = &info.folder_path else {
        ui.add(egui::Label::new(text).truncate()).on_hover_text(&info.folder);
        return;
    };
    let resp = ui.add(egui::Label::new(text).truncate().show_tooltip_when_elided(false).sense(Sense::click()));
    // (Checked only when used, not every frame: the folder may be on a
    // slow drive.)
    let exists = || path.is_dir();
    let resp = if resp.hovered() && !exists() {
        resp.on_hover_text(format!("{}\nFolder not found. Right-click to copy the path.", info.folder))
    } else {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\nClick to open. Right-click to copy the path.", info.folder))
    };
    if resp.clicked() && exists() {
        open_folder(path);
    }
    resp.context_menu(|ui| {
        if ui.add_enabled(exists(), egui::Button::new("Open folder")).on_disabled_hover_text("Folder not found").clicked() {
            open_folder(path);
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(info.folder.clone());
            ui.close();
        }
    });
}

/// Opens `path` in the system's file manager, without waiting for it.
fn open_folder(path: &std::path::Path) {
    let _ = open::that_detached(path);
}

/// What the user did in the session box.
#[derive(Default)]
pub(crate) struct BoxActions {
    effort: Option<String>,
    /// A model to switch to: its provider and id.
    model: Option<(api::Provider, String)>,
    goal: Option<String>,
    /// Pause (true) or resume the goal's auto-continue.
    goal_pause: Option<bool>,
    stop_beat: Option<u64>,
    remove_meter: Option<u64>,
    /// A subagent (or a subagent's main session) to open in its tab.
    open: Option<String>,
    edit_start: bool,
    edit_end: bool,
    /// Rename the conversation (`rename.rs`).
    rename: bool,
}

impl App {
    /// The session box, in the chat's top right corner: title, folder,
    /// model, the effort (a click opens the levels), the goal (a click
    /// edits it) and the model's heartbeats. Hidden when the window is too
    /// narrow for it beside the transcript.
    pub(crate) fn session_box(&mut self, ctx: &egui::Context, t: &Theme, area: Rect) {
        // A conversation's only, in the top left corner, clear of the transcript.
        let Screen::Chat(id) = &self.screen else { return };
        let id = id.clone();
        // A plugin's tab: its own box, if it has one.
        if let Some(kind) = crate::plugins::tab_kind(&id) {
            return kind.corner(ctx, self, &id, area);
        }
        if cloud::is_key(&id) {
            return self.cloud_box(ctx, t, area, &id);
        }
        let Some(w) = corner_width(area) else { return };
        let key = self.draft_key.clone();
        let models = match &*self.models.lock().unwrap() {
            Some(Ok(list)) => list.clone(),
            _ => vec![],
        };
        if let Some(s) = self.sessions.get(&key) {
            // Rewrite notes (Unslop): new and changed notes go to be rewritten.
            unslop::start_notes(&s.conv, ctx);
        }
        let mut info = SessionInfo::new(self.sessions.get(&key), &self.settings, models, |m| self.model_name(m));
        for a in &mut info.agents {
            a.3 = self.sessions.get(&a.2).map_or(AgentState::Idle, AgentState::of);
        }
        if let Some((_, conv, title)) = &mut info.parent {
            *title = self.sessions.get(conv.as_str()).map(|s| s.conv.lock().unwrap().title.clone()).unwrap_or_default();
        }
        let mut act = BoxActions::default();
        let editing = self.goal_edit.as_mut().filter(|g| g.0 == key);
        let tasks = self.sessions.get(&key).map(|s| s.tasks.clone());
        let bottom = area.bottom() - GAP;
        corner_area(ctx, "session-box", area, |ui| {
            panel_box(ui, t, w, 10, 8.0, |ui| {
                session_rows(ui, t, &info, editing, &mut act);
            });
            if let Some(tasks) = tasks {
                side_boxes(ui, t, w, bottom, &key, &info, &tasks);
            }
        });
        self.box_actions(ctx, t, &key, &info, act);
    }

    /// Does what the session box of conversation `key` (or of the new
    /// screen) asked for; `info` is what it showed.
    fn box_actions(&mut self, ctx: &egui::Context, t: &Theme, key: &str, info: &SessionInfo, act: BoxActions) {
        if act.edit_start {
            self.goal_edit = Some((key.to_string(), info.goal.clone(), true));
        }
        if act.edit_end {
            self.goal_edit = None;
        }
        if act.rename {
            self.start_rename(key);
        }
        if let Some((p, m)) = act.model.filter(|(p, m)| *m != info.model_id || *p != info.provider) {
            if self.sessions.contains_key(key) {
                // A conversation's switch asks first (`model_switch_dialog`).
                self.model_switch = Some((key.to_string(), p, m));
            } else {
                self.settings.provider = Some(p);
                self.settings.model = m;
                store::save_settings(&self.settings);
            }
        }
        self.model_switch_dialog(ctx, t, key);
        if let Some(e) = act.effort.filter(|e| *e != info.effort) {
            match self.sessions.get(key) {
                Some(s) => s.command(Command::Effort(e)),
                None => {
                    self.settings.effort = e;
                    store::save_settings(&self.settings);
                }
            }
        }
        if let Some(g) = act.goal.filter(|g| *g != info.goal) {
            match self.sessions.get(key) {
                Some(s) => s.command(Command::Goal(g)),
                // Before a conversation, a goal starts one (with the
                // message being written, if any).
                None if !g.is_empty() => self.send_input(|app, text, images| app.start_conversation(text, images, Some(Command::Goal(g)), ctx)),
                None => {}
            }
        }
        if let (Some(beat), Some(s)) = (act.stop_beat, self.sessions.get(key)) {
            s.stop_heartbeat(beat);
        }
        if let (Some(meter), Some(s)) = (act.remove_meter, self.sessions.get(key)) {
            s.remove_meter(meter);
        }
        if let (Some(on), Some(s)) = (act.goal_pause, self.sessions.get(key)) {
            s.set_goal_paused(on);
        }
        if let Some(id) = act.open.filter(|id| self.sessions.contains_key(id)) {
            self.open(id);
        }
    }
}

impl App {
    /// Asks before switching conversation `key` to another model: the new
    /// model has none of the conversation in its prompt cache, so its first
    /// request reads all of it again, at the full input price. Switch sends
    /// the change (`Command::Model`); Cancel, Esc or a click outside drops it.
    /// A switch asked for in another tab is dropped when this one shows.
    fn model_switch_dialog(&mut self, ctx: &egui::Context, t: &Theme, key: &str) {
        let Some((conv, provider, model)) = self.model_switch.clone() else { return };
        let Some(s) = self.sessions.get(&conv).filter(|_| conv == key) else {
            self.model_switch = None;
            return;
        };
        let context = s.conv.lock().unwrap().rolling.context;
        let name = self.model_name(&model);
        let mut choice = None;
        let modal = egui::Modal::new(Id::unique("model-switch"))
            .frame(egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(format!("Switch to {name}?")).strong().color(t.ink));
                ui.add_space(8.0);
                let size = if context > 0 { format!(" (about {}k tokens)", rolling::fmt_thousands(context)) } else { String::new() };
                let text = format!(
                    "The next request rereads the context{size} at full input price and may take longer. Conversation history is preserved."
                );
                ui.label(RichText::new(text).color(t.ink2));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.add(Button::new(RichText::new("Switch").color(t.accent))).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
        if modal.should_close() && choice.is_none() {
            choice = Some(false);
        }
        if let Some(switch) = choice {
            if switch {
                s.command(Command::Model(provider, model));
            }
            self.model_switch = None;
        }
    }
}

/// Notes and tasks under the session box, for a conversation: each a
/// drop-down. Open ones share the height left down to `bottom`.
fn side_boxes(ui: &mut Ui, t: &Theme, w: f32, bottom: f32, key: &str, info: &SessionInfo, tasks: &tasks::Tasks) {
    let notes_open = side_open(ui, "notes") && !info.notes.is_empty();
    let task_list = tasks.infos();
    let tasks_open = side_open(ui, "tasks") && !task_list.is_empty();
    let left = bottom - ui.min_rect().bottom() - 2.0 * (SIDE_HEAD + 8.0 + 16.0);
    let list_h = (left / (notes_open as u8 + tasks_open as u8).max(1) as f32).max(80.0);
    // A local model has no notes (`tools::LOCAL_TOOLS`).
    if info.provider != api::Provider::Local {
        notes_box(ui, t, w, &info.notes, info.parent.is_none(), list_h);
    }
    tasks_box(ui, t, w, key, tasks, &task_list, list_h);
}

/// Under the goal: pause or resume its auto-continue. Paused, the goal
/// stays (the model still has it) but OAK doesn't ask the model to carry
/// on when it ends its turn; the model isn't told.
fn goal_pause_row(ui: &mut Ui, t: &Theme, paused: bool, act: &mut BoxActions) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::click());
    let lit = resp.hovered();
    let p = ui.painter();
    let icon = Rect::from_center_size(egui::pos2(r.left() + 6.0, r.center().y), vec2(12.0, 12.0));
    let color = if paused { t.accent } else if lit { t.ink } else { t.ink3 };
    if paused {
        let c = icon.center();
        p.add(egui::Shape::convex_polygon(vec![c + vec2(-3.0, -4.5), c + vec2(4.5, 0.0), c + vec2(-3.0, 4.5)], color, Stroke::NONE));
    } else {
        paint_pause(p, icon, color);
    }
    let (text, text_color) = if paused { ("goal paused, resume", if lit { t.ink } else { t.ink2 }) } else { ("pause goal", color) };
    p.text(egui::pos2(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, text, font, text_color);
    let tip = if paused { "Resume automatic goal continuation" } else { "Pause automatic goal continuation; current work continues." };
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
        act.goal_pause = Some(!paused);
    }
}

/// A session box row that opens a menu: `key` (if any) faint, then `value`
/// and a chevron. Returns the row and whether its menu is open.
fn dropdown_row(ui: &mut Ui, t: &Theme, key: Option<&str>, value: &str) -> (egui::Response, bool) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let (r, resp) = row_rect(ui);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let lit = resp.hovered() || open;
    let x = match key {
        Some(k) => {
            ui.painter().text(egui::pos2(r.left(), r.center().y), Align2::LEFT_CENTER, k, font.clone(), t.ink3);
            r.left() + 44.0
        }
        None => r.left(),
    };
    let shown = clip_width(ui, value, &font, (r.right() - x - 18.0).max(0.0));
    let p = ui.painter();
    let g = p.text(egui::pos2(x, r.center().y), Align2::LEFT_CENTER, shown, font, if lit { t.ink } else { t.ink2 });
    chevron_v(p, egui::pos2(g.right() + 9.0, r.center().y), false, if lit { t.ink } else { t.ink3 });
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), open)
}

/// A choice in a session box menu, the one `on` in the accent. True when clicked.
fn choice(ui: &mut Ui, t: &Theme, on: bool, label: &str) -> bool {
    ui.selectable_label(on, RichText::new(label).color(if on { t.accent } else { t.ink })).clicked()
}

/// A session box row's rect and response: as wide as the box, 18 tall.
fn row_rect(ui: &mut Ui) -> (Rect, egui::Response) {
    ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::click())
}

/// A row of the session box (`key` faint, then `value` in `color`) that
/// opens `conv` when clicked, `tip` on hover.
fn open_row(ui: &mut Ui, t: &Theme, font: &FontId, key: &str, value: &str, color: Color32, tip: String) -> bool {
    let resp = ui.add(egui::Label::new(labeled(t, font, key, value, color)).truncate().sense(Sense::click()));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
}

/// `key` in the accent then `value` in `color`, both in `font`.
fn labeled(t: &Theme, font: &FontId, key: &str, value: &str, color: Color32) -> egui::text::LayoutJob {
    let fmt = |color| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut job = egui::text::LayoutJob::default();
    job.append(key, 0.0, fmt(t.accent));
    job.append(value, 0.0, fmt(color));
    job
}

/// How a model reads in the pickers: Freebuff's say when they aren't on
/// the account's plan, a Claude Fable one until when a limited-time offer
/// includes it (`api::fable_offer`).
pub(crate) fn model_label(m: &api::Model) -> String {
    if m.provider == api::Provider::Claude {
        return match api::fable_offer(&m.id, &api::accounts(), api::now_ms()) {
            Some((note, _)) if !note.is_empty() => format!("{} · {note}", m.name),
            _ => m.name.clone(),
        };
    }
    if m.provider != api::Provider::Freebuff {
        return m.name.clone();
    }
    let plan = if crate::freebuff::premium(&m.id) {
        ", paid plan"
    } else if crate::freebuff::opens_at(&m.id).is_some() {
        ", coming soon"
    } else if !crate::freebuff::in_catalog(&m.id) {
        ", not on this plan"
    } else {
        ""
    };
    format!("{}{plan}", m.name)
}

/// What a model's entry in the pickers says on hover: how the Claude
/// accounts' plans pay for a Fable model.
pub(crate) fn model_tip(m: &api::Model) -> Option<String> {
    (m.provider == api::Provider::Claude).then(|| api::fable_offer(&m.id, &api::accounts(), api::now_ms()))?.map(|(_, tip)| tip)
}

/// A picker entry for model `m` (`choice`, with its `model_tip`).
fn model_choice(ui: &mut Ui, t: &Theme, on: bool, m: &api::Model) -> bool {
    let resp = ui.selectable_label(on, RichText::new(model_label(m)).color(if on { t.accent } else { t.ink }));
    match model_tip(m) {
        Some(tip) => resp.on_hover_text(tip).clicked(),
        None => resp.clicked(),
    }
}

/// The session box's provider row: its logo and name, and the chevron.
fn provider_row(ui: &mut Ui, t: &Theme, p: api::Provider) -> (egui::Response, bool) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let (r, resp) = row_rect(ui);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let lit = resp.hovered() || open;
    // The logo centered on the name's letters (not on the row, whose
    // text sits a little low).
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(p.name().to_string(), font, if lit { t.ink } else { t.ink2 });
    let text = Align2::LEFT_CENTER.anchor_size(egui::pos2(r.left() + 18.0, r.center().y), galley.size());
    let logo = Rect::from_center_size(egui::pos2(r.left() + 6.0, text.center().y), vec2(12.0, 12.0));
    crate::providers_view::paint_logo(ui, p, logo);
    painter.galley(text.min, galley, t.ink2);
    chevron_v(painter, egui::pos2(text.right() + 9.0, r.center().y), false, if lit { t.ink } else { t.ink3 });
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), open)
}

/// The session box's rows.
pub(crate) fn session_rows(ui: &mut Ui, t: &Theme, info: &SessionInfo, editing: Option<&mut (String, String, bool)>, act: &mut BoxActions) {
    ui.spacing_mut().item_spacing.y = 3.0;
    title_row(ui, t, info, act);
    folder_row(ui, info, RichText::new(&info.folder).small().color(t.ink3));
    let font = egui::TextStyle::Small.resolve(ui.style());
    model_rows(ui, t, info, act);
    let notes = [
        // Freebuff (and the cloud sessions) run their own agent and system
        // prompt: Oak is only the pipe to them.
        (info.provider == api::Provider::Freebuff || info.note_separate_system).then_some("Uses its own agent and system prompt"),
        // A model on the user's own server gets the basics only (`local`).
        (info.provider == api::Provider::Local).then_some("Limited tools; no notes, heartbeats, progress meters, subagents, or screenshots."),
    ];
    for note in notes.into_iter().flatten() {
        ui.add_space(3.0);
        ui.add(egui::Label::new(RichText::new(note).small().color(t.ink3)).wrap());
    }
    ui.add_space(4.0);
    goal_rows(ui, t, info, &font, editing, act);
    if !info.meters.is_empty() {
        ui.add_space(4.0);
        meter_rows(ui, t, &info.meters, act);
    }
    link_rows(ui, t, info, &font, act);
}

/// The session box's title. A conversation's own (not a subagent's): a
/// double-click or the right-click menu renames it.
fn title_row(ui: &mut Ui, t: &Theme, info: &SessionInfo, act: &mut BoxActions) {
    let title = RichText::new(&info.title).color(if info.new { t.ink2 } else { t.ink }).strong();
    if info.new || info.parent.is_some() || info.note_separate_system {
        ui.add(egui::Label::new(title).truncate()).on_hover_text(&info.title);
    } else {
        let resp = ui.add(egui::Label::new(title).truncate().selectable(false).sense(Sense::click())).on_hover_text(format!("{}\nDouble-click to rename", info.title));
        if resp.double_clicked() {
            act.rename = true;
        }
        resp.context_menu(|ui| {
            if ui.button("Rename…").clicked() {
                act.rename = true;
                ui.close();
            }
        });
    }
}

/// The session box's provider, model and effort, each a drop-down.
fn model_rows(ui: &mut Ui, t: &Theme, info: &SessionInfo, act: &mut BoxActions) {
    // The provider: a click opens the providers, each with its models to
    // the right (a provider is chosen with one of its models).
    let (resp, open) = provider_row(ui, t, info.provider);
    let resp = if open || info.new || info.models.is_empty() { resp } else { resp.on_hover_text("Switching provider resets the prompt cache") };
    egui::Popup::menu(&resp).width(160.0).show(|ui| {
        for provider in api::Provider::ALL {
            let group: Vec<&api::Model> = info.models.iter().filter(|m| m.provider == provider).collect();
            if group.is_empty() {
                continue;
            }
            let label = RichText::new(provider.name()).color(if provider == info.provider { t.accent } else { t.ink });
            ui.menu_image_text_button(crate::providers_view::logo_image(ui.ctx(), provider, 14.0), label, |ui| {
                for m in group {
                    if model_choice(ui, t, info.provider == provider && info.model_id == m.id, m) {
                        act.model = Some((provider, m.id.clone()));
                    }
                }
            });
        }
        if info.models.is_empty() {
            caption(ui, "Loading models…", t.ink3);
        }
    });

    // The model: a click opens the provider's models.
    let model = if info.model.is_empty() { "no model" } else { info.model.as_str() };
    let (resp, open) = dropdown_row(ui, t, None, model);
    let resp = if open || info.new || info.models.is_empty() { resp } else { resp.on_hover_text("Switching model resets the prompt cache") };
    egui::Popup::menu(&resp).width(200.0).show(|ui| {
        // A label with a note ("· Included until …") widens the menu.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        let group: Vec<&api::Model> = info.models.iter().filter(|m| m.provider == info.provider).collect();
        for m in &group {
            if model_choice(ui, t, info.model_id == m.id, m) {
                act.model = Some((info.provider, m.id.clone()));
            }
        }
        if group.is_empty() {
            let text = if info.models.is_empty() { "Loading models…".to_string() } else { format!("No {} models: add an account on Home", info.provider.name()) };
            caption(ui, &text, t.ink3);
        }
    });

    // The effort: a click opens the levels.
    let shown = if info.efforts.is_empty() { "none" } else { info.effort.as_str() };
    let (resp, open) = dropdown_row(ui, t, Some("effort"), shown);
    let resp = if !open && info.efforts.is_empty() { resp.on_hover_text("Not supported by this model") } else { resp };
    egui::Popup::menu(&resp).width(120.0).show(|ui| {
        for &e in &info.efforts {
            if choice(ui, t, info.effort == e, e) {
                act.effort = Some(e.to_string());
            }
        }
        if info.efforts.is_empty() {
            caption(ui, "Not supported by this model", t.ink3);
        }
    });
}

/// The session box's links: the parent, the subagents and the heartbeats.
fn link_rows(ui: &mut Ui, t: &Theme, info: &SessionInfo, font: &FontId, act: &mut BoxActions) {
    // A subagent: the session that started it; a click opens it.
    if let Some((n, conv, title)) = &info.parent {
        let title = if title.is_empty() { "Untitled" } else { title };
        let tip = format!("Parent session: {title}");
        if open_row(ui, t, font, "agent  ", &format!("{n} of {title}"), t.ink2, tip) {
            act.open = Some(conv.clone());
        }
    }
    // The subagents it started: a dot while one works, a spinner while it
    // pauses, an empty circle once paused; a click opens it.
    for (n, name, conv, state) in &info.agents {
        if agent_row(ui, t, font, *n, name, *state) {
            act.open = Some(conv.clone());
        }
    }

    for h in &info.heartbeats {
        let text = format!("every {}, {}", agent::every(h.every_min), one_line(&h.message));
        let resp = ui.add(egui::Label::new(labeled(t, font, "beat  ", &text, t.ink3)).truncate().sense(Sense::click()));
        let tip = format!("Heartbeat {}: {}\nNext in {}\nRight-click to stop it", h.id, h.message, until(h.next_at));
        resp.on_hover_text(tip).context_menu(|ui| {
            if ui.button("Stop heartbeat").clicked() {
                act.stop_beat = Some(h.id);
                ui.close();
            }
        });
    }
}

/// The session box's goal: a click edits it (`editing`: the text being
/// written, and whether it is to take focus); under it, pausing it.
fn goal_rows(ui: &mut Ui, t: &Theme, info: &SessionInfo, font: &FontId, editing: Option<&mut (String, String, bool)>, act: &mut BoxActions) {
    let job = |key: &str, value: &str, color: Color32| labeled(t, font, key, value, color);
    match editing {
        Some((_, text, focus)) => {
            caption(ui, "goal", t.accent);
            let id = Id::unique("goal-edit");
            let enter = enter_pressed(ui, id);
            let edit = crate::text_menu::add(
                ui,
                sending_edit(text, id)
                    .font(egui::TextStyle::Small)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(6, 4))
                    .hint_text("Goal"),
            );
            if std::mem::take(focus) {
                edit.request_focus();
            }
            let esc = left_with(ui, &edit, Key::Escape);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().button_padding = vec2(8.0, 1.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                let set = ui.button(RichText::new(if info.new { "Start" } else { "Set" }).small().color(t.accent)).clicked();
                if set || enter {
                    act.goal = Some(text.trim().to_string());
                    act.edit_end = true;
                }
                if ui.button(RichText::new("Cancel").small()).clicked() || esc {
                    act.edit_end = true;
                }
                if !info.goal.is_empty() && ui.button(RichText::new("Clear").small()).clicked() {
                    act.goal = Some(String::new());
                    act.edit_end = true;
                }
            });
        }
        None => {
            let label = if info.goal.trim().is_empty() {
                egui::Label::new(job("goal  ", "none", t.ink3))
            } else {
                egui::Label::new(job("goal  ", info.goal.trim(), t.ink2)).wrap()
            };
            let resp = ScrollArea::vertical()
                .id_salt("session-goal")
                .max_height(110.0)
                .show(ui, |ui| ui.add(label.sense(Sense::click())))
                .inner;
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
            if resp.on_hover_text("Edit goal").clicked() {
                act.edit_start = true;
            }
            if !info.new && !info.goal.trim().is_empty() {
                goal_pause_row(ui, t, info.goal_paused, act);
            }
        }
    }
}

/// A subagent's row: "agent", its state (`AgentState`), its number and
/// name. True when clicked (it opens).
fn agent_row(ui: &mut Ui, t: &Theme, font: &FontId, n: u64, name: &str, state: AgentState) -> bool {
    let (r, resp) = row_rect(ui);
    let lit = resp.hovered();
    let p = ui.painter();
    let g = p.text(egui::pos2(r.left(), r.center().y), Align2::LEFT_CENTER, "agent", font.clone(), t.accent);
    let mut x = g.right() + 8.0;
    let dot = egui::pos2(x + 3.0, r.center().y);
    match state {
        AgentState::Idle => {}
        AgentState::Working => state_dot(p, t, dot, false),
        AgentState::Pausing => {
            egui::Spinner::new().size(9.0).color(t.ink3).paint_at(ui, Rect::from_center_size(dot, vec2(9.0, 9.0)));
        }
        AgentState::Paused => state_dot(p, t, dot, true),
    }
    if state != AgentState::Idle {
        x += 12.0;
    }
    let label = clip_width(ui, &format!("{n}, {name}"), font, r.right() - x);
    ui.painter().text(egui::pos2(x, r.center().y), Align2::LEFT_CENTER, label, font.clone(), if lit { t.ink } else { t.ink2 });
    let state = match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Pausing => "pausing",
        AgentState::Paused => "paused",
    };
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Agent {n}: {name} ({state})")).clicked()
}

/// The progress meters, under the goal: the label on the left, the value
/// on the right (a red "!" before it when the last run failed), and a thin
/// bar under x/y and N% values. Hover one for its note, error and command;
/// right-click to remove it.
fn meter_rows(ui: &mut Ui, t: &Theme, meters: &[store::Meter], act: &mut BoxActions) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    for m in meters {
        let bar = m.value.as_ref().and_then(|v| v.fraction);
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), if bar.is_some() { 22.0 } else { 18.0 }), Sense::click());
        let y = r.top() + 9.0;
        let (value, color) = match &m.value {
            Some(v) => (v.value.as_str(), t.ink),
            None if m.error.is_empty() => ("…", t.ink3),
            None => ("–", t.ink3),
        };
        let p = ui.painter();
        let mut right = p.text(egui::pos2(r.right(), y), Align2::RIGHT_CENTER, value, font.clone(), color).left();
        if !m.error.is_empty() {
            right = p.text(egui::pos2(right - 5.0, y), Align2::RIGHT_CENTER, "!", font.clone(), t.error).left();
        }
        let label = clip_width(ui, &m.label, &font, (right - 10.0 - r.left()).max(0.0));
        p.text(egui::pos2(r.left(), y), Align2::LEFT_CENTER, label, font.clone(), t.ink2);
        if let Some(f) = bar {
            let track = Rect::from_min_size(egui::pos2(r.left(), r.bottom() - 4.0), vec2(r.width(), 2.0));
            p.rect_filled(track, 1.0, t.line);
            p.rect_filled(Rect::from_min_size(track.min, vec2(track.width() * f as f32, 2.0)), 1.0, t.accent);
        }
        let mut tip = m.label.clone();
        if let Some(note) = m.value.as_ref().map(|v| v.note.as_str()).filter(|n| !n.is_empty()) {
            tip += &format!("\n{note}");
        }
        if !m.error.is_empty() {
            tip += &format!("\n\nLast run failed: {}", m.error);
        }
        let last = if m.ran_at == 0 { "first run under way".to_string() } else { format!("last run {}", ago(m.ran_at)) };
        tip += &format!("\n\n$ {}\nEvery {}; {last}\nRight-click to remove it", m.command, agent::every(m.every_min));
        resp.on_hover_text(tip).context_menu(|ui| {
            if ui.button("Remove meter").clicked() {
                act.remove_meter = Some(m.id);
                ui.close();
            }
        });
    }
}

/// The header row of the notes and tasks boxes.
const SIDE_HEAD: f32 = 20.0;
/// The most output lines a task shows when opened in the tasks box.
const TASK_TAIL: usize = 400;

/// Whether the drop-down box `name` ("notes", "tasks") is open.
fn side_open(ui: &Ui, name: &str) -> bool {
    is_open(ui, Id::unique(("side-open", name)), false)
}

/// A drop-down box's header (`widgets::drop_head`); with nothing in it, it
/// stays closed.
fn side_head(ui: &mut Ui, t: &Theme, name: &str, title: &str, count: &str, right: (&str, Color32), enabled: bool) -> (bool, egui::Response) {
    crate::widgets::drop_head(ui, t, Id::unique(("side-open", name)), SIDE_HEAD, title, count, right, enabled)
}

/// "640 B", "5.9 KB": the size of the notes.
fn note_size(tokens: u64) -> String {
    format!("{} tokens", crate::rolling::fmt_tokens(tokens))
}

/// The model's notes (NOTE action=new), in a drop-down box. Its size turns red
/// once the model is told to tidy them up. With Rewrite notes, the notes
/// of the user's `own` session show rewritten (the size stays the
/// model's notes').
pub(crate) fn notes_box(ui: &mut Ui, t: &Theme, w: f32, notes: &[store::Note], own: bool, max_h: f32) {
    let tokens = agent::notes_tokens(notes);
    let over = tokens > agent::NOTES_ALERT_TOKENS && agent::notes_alert_on();
    panel_box(ui, t, w, 6, 3.0, |ui| {
        let count = if notes.is_empty() { "none".to_string() } else { notes.len().to_string() };
        let size = if notes.is_empty() { String::new() } else { note_size(tokens) };
        let (open, resp) = side_head(ui, t, "notes", "Notes", &count, (&size, if over { t.error } else { t.ink3 }), !notes.is_empty());
        let tip = if notes.is_empty() {
            "No session notes".to_string()
        } else if over {
            format!("About {} of notes; cleanup threshold {}", note_size(tokens), note_size(agent::NOTES_ALERT_TOKENS))
        } else {
            "Session notes, included in every request".to_string()
        };
        resp.on_hover_text(tip);
        if !open {
            return;
        }
        ui.add_space(2.0);
        ScrollArea::vertical().id_salt("side-notes").max_height(max_h).min_scrolled_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let font = egui::TextStyle::Small.resolve(ui.style());
            for n in notes {
                let ink = |color| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
                let state = unslop::note_state(n, own);
                let (text, color, failed) = match &state {
                    None => (n.text.trim(), t.ink2, None),
                    Some(unslop::State::Done(s)) => (s.trim(), t.ink2, None),
                    Some(unslop::State::Streaming(s)) if !s.trim().is_empty() => (s.trim(), t.ink2, None),
                    Some(unslop::State::Failed(e)) => (n.text.trim(), t.ink2, Some(e)),
                    Some(_) => ("Rewriting…", t.ink3, None),
                };
                let mut job = egui::text::LayoutJob::default();
                job.append(&format!("{}  ", n.id), 0.0, ink(t.accent));
                job.append(text, 0.0, ink(color));
                let r = ui.add(egui::Label::new(job).wrap());
                if let Some(e) = failed {
                    r.on_hover_text(format!("Rewrite failed: {e}"));
                }
            }
        });
        ui.add_space(2.0);
    });
}

/// The session's background tasks in a drop-down box: running ones first
/// (newest first), then finished ones, greyed out. A click on a task shows
/// its command and output.
fn tasks_box(ui: &mut Ui, t: &Theme, w: f32, conv: &str, tasks: &tasks::Tasks, list: &[tasks::TaskInfo], max_h: f32) {
    let running = list.iter().filter(|i| i.ended.is_none()).count();
    panel_box(ui, t, w, 6, 3.0, |ui| {
        let count = if list.is_empty() { "none".to_string() } else { list.len().to_string() };
        let right = if running > 0 { format!("{running} running") } else { String::new() };
        let (open, resp) = side_head(ui, t, "tasks", "Tasks", &count, (&right, t.accent), !list.is_empty());
        resp.on_hover_text(if list.is_empty() {
            "No background tasks"
        } else {
            "Background tasks, commands and output"
        });
        if !open {
            return;
        }
        if running > 0 {
            // Running times and output keep moving.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        ui.add_space(2.0);
        ScrollArea::vertical().id_salt("side-tasks").max_height(max_h).min_scrolled_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for i in list {
                task_row(ui, t, conv, tasks, i);
            }
        });
        ui.add_space(2.0);
    });
}

/// A task in the tasks box; open, its command and output under it.
fn task_row(ui: &mut Ui, t: &Theme, conv: &str, tasks: &tasks::Tasks, i: &tasks::TaskInfo) {
    let id = Id::unique(("side-task", conv, i.id));
    let mut open = is_open(ui, id, false);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
    let lit = resp.hovered();
    if resp.clicked() {
        open = !open;
        set_open(ui, id, open);
    }
    let done = i.ended.is_some();
    let failed = i.exit.is_some_and(|c| c != 0) && !i.removed;
    let p = ui.painter();
    if lit {
        p.rect_filled(r, 0.0, t.panel2);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let y = r.center().y;
    let dot = egui::pos2(r.left() + 6.0, y);
    match (done, failed) {
        (false, _) => {
            p.circle_filled(dot, 3.0, t.accent);
        }
        (true, true) => {
            let c = mix(t.panel, t.error, 0.8);
            p.line_segment([dot + vec2(-3.0, -3.0), dot + vec2(3.0, 3.0)], Stroke::new(1.4, c));
            p.line_segment([dot + vec2(-3.0, 3.0), dot + vec2(3.0, -3.0)], Stroke::new(1.4, c));
        }
        (true, false) => {
            p.circle_stroke(dot, 3.0, Stroke::new(1.2, t.ink4));
        }
    };
    let small = egui::TextStyle::Small.resolve(ui.style());
    let time = p.text(egui::pos2(r.right() - 4.0, y), Align2::RIGHT_CENTER, tasks::fmt_duration(i.elapsed), small.clone(), if done { t.ink4 } else { t.ink3 });
    let label = if i.summary.is_empty() { i.command.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim() } else { &i.summary };
    let x = r.left() + 16.0;
    let text = clip_width(ui, &format!("{}, {label}", i.id), &small, time.left() - 8.0 - x);
    ui.painter().text(egui::pos2(x, y), Align2::LEFT_CENTER, text, small, if done { t.ink3 } else if lit { t.ink } else { t.ink2 });
    let state = if i.removed { format!("{}; removed", i.status) } else { i.status.clone() };
    resp.on_hover_text(format!("Task {}: {state}\n{label}", i.id));
    if !open {
        return;
    }
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let code = markdown::code_font(11.5);
            let ink = if done { t.ink3 } else { t.ink2 };
            let state = if i.removed { format!("{}, removed by the model", i.status) } else { i.status.clone() };
            ui.label(RichText::new(state).small().color(if failed { t.error } else { t.ink3 }));
            inset(ui, t, t.bg, 6, |ui| ui.add(egui::Label::new(RichText::new(format!("$ {}", i.command.trim_end())).font(code.clone()).color(ink)).wrap()));
            let (lines, total) = tasks.output(i.id, TASK_TAIL).unwrap_or_default();
            if lines.is_empty() {
                caption(ui, "no output", t.ink3);
            } else {
                inset(ui, t, t.bg, 6, |ui| {
                    ScrollArea::vertical().id_salt(("task-output", conv, i.id)).max_height(220.0).min_scrolled_height(220.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(lines.join("\n")).font(code.clone()).color(ink)).wrap());
                    });
                });
                if total > lines.len() {
                    caption(ui, &format!("Last {} of {total} lines", lines.len()), t.ink3);
                }
            }
        });
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The folder row opens and copies only a folder that was chosen.
    #[test]
    fn folder_row_has_a_path_only_when_a_folder_is_chosen() {
        let mut settings = Settings::default();
        let info = SessionInfo::new(None, &settings, vec![], |m| m.to_string());
        assert_eq!(info.folder, NO_FOLDER);
        assert!(info.folder_path.is_none());
        settings.folder = " /work/oak ".into();
        let info = SessionInfo::new(None, &settings, vec![], |m| m.to_string());
        assert_eq!(info.folder_path, Some(std::path::PathBuf::from("/work/oak")));
    }
}
