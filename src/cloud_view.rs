//! Cloud sessions (cloud.rs) on screen: Home's cloud list, a cloud session's
//! tab, and its box in the corner.

use super::*;
use crate::widgets::{clip_width, error_note, working};
use crate::composer::GAP;
use crate::session_box::{corner_area, corner_width, panel_box};
use crate::home::{ChatRow, chat_list};

/// Nerd Font: a cloud.
pub(crate) const CLOUD_ICON: &str = "\u{f0c2}";

/// The icon of a cloud session of `provider`, and the font it is in at
/// `size` (a plugin's may be a character of the text font).
pub(crate) fn provider_icon(provider: cloud::Provider, size: f32) -> (String, FontId) {
    let (icon, text) = provider.icon();
    (icon, if text { FontId::proportional(size + 2.0) } else { markdown::icon_font(size) })
}

/// Home's list of cloud sessions, every account's, fetched while it shows.
/// Returns the one clicked.
pub(crate) fn cloud_list_view(ui: &mut Ui, t: &Theme, provider: cloud::Provider) -> Option<String> {
    cloud::want_list(provider, ui.ctx());
    let (list, loading, error, listed) = cloud::list(provider);
    let mut r = ui.max_rect();
    if let Some(e) = &error {
        let line = Rect::from_min_max(egui::pos2(r.left(), r.bottom() - 24.0), r.max);
        ui.painter().text(egui::pos2(line.left() + 12.0, line.center().y), Align2::LEFT_CENTER, clip_width(ui, e, &FontId::proportional(11.5), line.width() - 24.0), FontId::proportional(11.5), t.error);
        r.max.y = line.top();
    }
    if list.is_empty() {
        let none = format!("No {} sessions", provider.name().to_lowercase());
        let text = if loading || !listed { "Loading cloud sessions…" } else if error.is_some() { "" } else { none.as_str() };
        crate::widgets::center_note(ui, t, r, text);
        return None;
    }
    let icon = provider.icon().0;
    let rows: Vec<ChatRow> = list
        .iter()
        .map(|x| ChatRow {
            id: x.key.clone(),
            title: if x.title.is_empty() { "Untitled".into() } else { x.title.clone() },
            folder: format!("{icon} {}{}", x.account, if x.status == "requires_action" { ", waiting for you" } else { "" }),
            at: x.updated,
            busy: x.status == "running",
            paused: false,
            renamable: false,
        })
        .collect();
    ui.scope_builder(UiBuilder::new().max_rect(r), |ui| chat_list(ui, t, &rows, false)).inner.open
}

/// The New screen's lines about a cloud session through `provider`: what
/// it is, and why none can start now.
pub(crate) fn cloud_new_info(ui: &mut Ui, t: &Theme, provider: cloud::Provider) {
    let wrap = |ui: &mut Ui, text: &str, color: Color32| {
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    };
    let about = provider.about();
    if !about.is_empty() {
        wrap(ui, &about, t.ink2);
    }
    ui.add_space(22.0);
    if cloud::starting() {
        working(ui, t, "Starting the cloud session…");
    } else if let Some(why) = provider.unavailable() {
        wrap(ui, &why, t.error);
    }
}

/// A cloud session's transcript, drawn as a local conversation's is, from
/// its events.
pub(crate) fn cloud_chat_view(ui: &mut Ui, t: &Theme, key: &str, list: &mut vlist::List) {
    let Some(chat) = cloud::chat(key) else { return };
    let c = chat.lock().unwrap();
    let tail = crate::hash_of(&(c.notes.len(), c.working, &c.status, c.pending.is_some(), &c.trouble, c.connected, c.conv.messages.is_empty()));
    chat_view::read_only_view(ui, t, &c.conv, &c.live, list, tail, |ui| cloud_tail(ui, t, &c));
}

/// The end of a cloud session's transcript: what went wrong lately, and
/// what it is doing.
fn cloud_tail(ui: &mut Ui, t: &Theme, c: &cloud::Chat) {
    for (text, bad) in c.notes.iter().rev().take(3).rev() {
        ui.add(egui::Label::new(RichText::new(text).small().color(if *bad { t.error } else { t.ink3 })).wrap());
        ui.add_space(4.0);
    }
    if !c.connected && !c.trouble.is_empty() {
        caption(ui, &c.trouble, t.ink3);
    } else if !c.connected && c.conv.messages.is_empty() {
        working(ui, t, "Loading…");
    }
    if c.working && c.pending.is_none() {
        working(ui, t, if c.status.is_empty() { "Working…" } else { &c.status });
    } else if let Some((_, tool, _)) = &c.pending {
        ui.label(RichText::new(format!("Approval required: {tool}, session panel")).color(t.accent));
    }
}

impl App {
    /// The tab shown is a cloud session, or the New screen starts one.
    pub(crate) fn cloud_screen(&self) -> bool {
        match &self.screen {
            Screen::Chat(id) => cloud::is_key(id),
            Screen::New => self.new_cloud,
        }
    }

    /// Starts a cloud session with the message; the message box empties at
    /// once and gets it back if that fails (`cloud_started`).
    pub(crate) fn start_cloud(&mut self, text: String, images: Vec<Value>, ctx: &egui::Context) {
        if cloud::starting() {
            self.hint = Some("A cloud session is starting already.".into());
            return;
        }
        if let Some(why) = self.new_provider.unavailable() {
            self.hint = Some(why);
            return;
        }
        self.cloud_pending = Some((self.draft_key.clone(), text.clone(), images.clone()));
        cloud::start(self.new_provider, text, images, ctx);
    }

    /// A cloud session was started (or moved): its tab opens. If it
    /// couldn't be, the message goes back where it was written.
    pub(crate) fn cloud_started(&mut self, r: Result<String, String>) {
        let pending = self.cloud_pending.take();
        match r {
            Ok(key) => {
                cloud::stale_list();
                self.open(key);
            }
            Err(e) => {
                if let Some((draft, text, attachments)) = pending {
                    if draft == self.draft_key {
                        if self.input.trim().is_empty() {
                            self.input = text;
                        }
                        self.attachments.splice(0..0, attachments);
                    } else {
                        self.drafts.entry(draft).or_insert((text, attachments));
                    }
                }
                self.hint = Some(e);
            }
        }
    }

    /// A cloud session's box, in the corner where a conversation's goes:
    /// whose it is and what it does, its question if it has one, moving it
    /// when its account is out, archiving it, and (with a kit) the files on
    /// its server.
    pub(crate) fn cloud_box(&mut self, ctx: &egui::Context, t: &Theme, area: Rect, key: &str) {
        let Some(w) = corner_width(area) else { return };
        let Some(chat) = cloud::chat(key) else { return };
        let (title, rid, working, pending, limited, moving, connected, sending) = {
            let c = chat.lock().unwrap();
            (c.title.clone(), c.rid.clone(), c.working, c.pending.clone(), c.limited_until, c.moving, c.connected, c.sending)
        };
        let title = if title.is_empty() { cloud::tab_info(key).0 } else { title };
        let provider = cloud::provider(key);
        let icon = provider.icon().0;
        let account = provider.accounts().into_iter().find(|a| a.rid == rid).map(|a| a.label).unwrap_or_else(|| "an account no longer signed in".into());
        let server = provider.server_name();
        let (files, files_error) = if cloud::accessible(key) { cloud::files(provider) } else { (vec![], provider.unavailable()) };
        let mut act = CloudAct::default();
        let now = api::now_ms();
        let bottom = area.bottom() - GAP;
        corner_area(ctx, "cloud-box", area, |ui| {
            panel_box(ui, t, w, 10, 4.0, |ui| {
                ui.add(egui::Label::new(RichText::new(format!("{icon}  {title}")).strong().color(t.ink)).truncate());
                ui.add(egui::Label::new(RichText::new(format!("{} session on {account}", provider.name())).small().color(t.ink3)).truncate());
                let state = if !connected {
                    "Connecting…"
                } else if pending.is_some() {
                    "Waiting for you"
                } else if sending {
                    "Sending…"
                } else if working {
                    "Working"
                } else {
                    "Idle"
                };
                ui.horizontal(|ui| {
                    caption(ui, state, t.ink2);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Archive").on_hover_text("Archive it: it leaves the list, here and everywhere else").clicked() {
                            act.archive = true;
                        }
                        if working && ui.small_button("Stop").clicked() {
                            act.stop = true;
                        }
                    });
                });
                if let Some((_, tool, input)) = &pending {
                    act.answer = approval(ui, t, tool, input);
                }
                if limited > now {
                    ui.add_space(4.0);
                    let movable = cloud::can_move(&rid);
                    let text = format!(
                        "Its account has reached its usage limit until {}.{}",
                        clock(limited),
                        if movable {
                            " Move it to another account to carry on now; what is in the message box goes with it."
                        } else if server.is_none() {
                            ""
                        } else {
                            " Every other account is at its limit too."
                        }
                    );
                    ui.add(egui::Label::new(RichText::new(text).small().color(t.ink2)).wrap());
                    if movable && ui.add_enabled(!moving && !cloud::starting(), Button::new(if moving { "Moving…" } else { "Move to another account" })).clicked() {
                        act.mv = true;
                    }
                }
            });
            if let Some(server) = &server {
                panel_box(ui, t, w, 10, 4.0, |ui| files_box(ui, t, bottom, server, &files, files_error.as_deref(), &mut act));
            }
        });
        self.do_cloud_act(ctx, key, provider, act);
    }

    /// Does what the cloud box asked for.
    fn do_cloud_act(&mut self, ctx: &egui::Context, key: &str, provider: cloud::Provider, act: CloudAct) {
        if act.stop {
            cloud::interrupt(key);
        }
        if let Some(allow) = act.answer {
            cloud::answer(key, allow);
        }
        if act.archive {
            cloud::archive(key, ctx);
            if let Some(i) = self.settings.tabs.iter().position(|k| k == key) {
                self.settings.tabs.remove(i);
                store::save_settings(&self.settings);
            }
            self.page = Page::Home;
            // Home, on the tab its sessions are listed in.
            self.home_list = if provider.is_own() { home::CLOUD_TAB.to_string() } else { provider.id() };
        }
        if act.mv {
            let text = std::mem::take(&mut self.input).trim().to_string();
            let attachments = std::mem::take(&mut self.attachments);
            self.cloud_pending = Some((self.draft_key.clone(), text.clone(), attachments.clone()));
            cloud::move_chat(key, text, attachments, ctx);
        }
        if let Some(name) = act.save
            && let Some(dest) = rfd::FileDialog::new().set_file_name(&name).save_file()
        {
            cloud::save_file(provider, &name, dest, ctx);
        }
        if let Some(name) = act.delete {
            cloud::delete_file(provider, &name, ctx);
        }
    }
}

/// What the cloud box was asked to do this frame.
#[derive(Default)]
struct CloudAct {
    stop: bool,
    answer: Option<bool>,
    mv: bool,
    archive: bool,
    save: Option<String>,
    delete: Option<String>,
}

/// The session's question: the tool waiting for approval, and Allow or
/// Deny. Gives the answer clicked.
fn approval(ui: &mut Ui, t: &Theme, tool: &str, input: &Value) -> Option<bool> {
    ui.add_space(4.0);
    ui.label(RichText::new(format!("Approval required: {tool}")).color(t.accent));
    let shown = tool_summary(tool, input);
    if !shown.is_empty() {
        ui.add(egui::Label::new(RichText::new(shown).small().monospace().color(t.ink2)).wrap());
    }
    let mut answer = None;
    ui.horizontal(|ui| {
        if ui.button("Allow").clicked() {
            answer = Some(true);
        }
        if ui.button("Deny").clicked() {
            answer = Some(false);
        }
    });
    answer
}

/// The files on the kit's server (`server`, as sentences name it): what
/// agents pushed, and what the user sent them, each to save or delete;
/// scrolling above `bottom`.
fn files_box(ui: &mut Ui, t: &Theme, bottom: f32, server: &str, files: &[cloud::File], error: Option<&str>, act: &mut CloudAct) {
    caption(ui, &format!("Files on {server}, {}", files.len()), t.ink3);
    if let Some(e) = error {
        error_note(ui, t, e);
    }
    if files.is_empty() {
        caption(ui, "No files", t.ink3);
    }
    let max_h = (bottom - ui.min_rect().bottom() - 40.0).max(80.0);
    ScrollArea::vertical().id_salt("cloud-files").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
        for f in files {
            ui.horizontal(|ui| {
                let name_w = ui.available_width() - 70.0;
                ui.allocate_ui(vec2(name_w, 18.0), |ui| {
                    ui.add(egui::Label::new(RichText::new(&f.name).small().color(t.ink2)).truncate())
                        .on_hover_text(format!("{}, {}, {}", f.name, size_text(f.size), full_time(f.modified, local_offset_ms())));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text(format!("Delete from {server}")).clicked() {
                        act.delete = Some(f.name.clone());
                    }
                    if ui.small_button("Save").on_hover_text("Save locally").clicked() {
                        act.save = Some(f.name.clone());
                    }
                });
            });
        }
    });
}
