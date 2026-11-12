//! Home, the first tab: the forest, and over it the box with the
//! conversations (or another of its tabs: Cloud, those plugins add,
//! Duck.ai) on the left and the accounts on the right.

use super::*;
pub(crate) use oak_sdk::ui::{BAR_ROOM, HOME_HEAD, list_rows, row_title_color};
use crate::api::Provider;
use crate::widgets::{add_field, clip_width, sign_in_heading, state_dot};

use crate::composer::{BOX_W, nodeclub_panel, openrouter_panel, side_box, usage_panel};

impl App {
    /// Home: the forest, and over it one solid box: the chats on the left
    /// and the signed-in accounts on the right, each under a small heading.
    /// Sign out is a small button under the box (Settings opens from the
    /// gear in a conversation's bottom left corner).
    pub(crate) fn home(&mut self, ui: &mut Ui, t: &Theme) {
        let rect = ui.max_rect();
        let tex = picture(ui.ctx(), &mut self.pictures, "forest-home");
        paint_cover(ui.painter(), rect, &tex, Color32::WHITE);
        let g = home_grid(rect);
        paint_grid(ui.painter(), t, &g, true);
        self.chats_head(ui, t, g.chats_head);
        cell_heading(ui.painter(), t, g.accounts_head, "Providers");
        version_label(ui, t, g.accounts_head);
        self.chats_cell(ui, t, g.chats);
        ui.scope_builder(UiBuilder::new().max_rect(g.accounts), |ui| self.account_column(ui, t));
        self.delete_chat_dialog(ui.ctx(), t);
        self.reset_dialog(ui.ctx(), t);
        // Under the accounts: what 1% of the 5-hour limit buys; a click
        // opens the Usage page.
        if let Some(r) = g.preview
            && usage_view::preview(ui, t, r, &mut self.usage)
        {
            self.page = Page::Usage;
        }
    }

    /// The left cell's tabs: this machine's conversations, the cloud
    /// sessions, and those plugins add (`oak_sdk::HomeTab`). Duck.ai, on
    /// the tabs' row against its right end, and left of it an update when
    /// there is one (`update.rs`); the tabs scroll in what is left.
    fn chats_head(&mut self, ui: &mut Ui, t: &Theme, head: Rect) {
        let (duck_w, duck) = duck_tab(ui, t, head);
        if duck {
            self.page = Page::Duck;
        }
        let right = (head.right() - duck_w).max(head.left());
        let update_w = update_tab(ui, t, Rect::from_min_max(head.min, egui::pos2(right, head.bottom())));
        let tabs_head = Rect::from_min_max(head.min, egui::pos2((right - update_w).max(head.left()), head.bottom()));
        if let Some(pick) = home_tabs(ui, t, tabs_head, &self.home_list) {
            self.home_list = pick;
        }
    }

    /// The left cell, in `chats`: the list its tab picks.
    fn chats_cell(&mut self, ui: &mut Ui, t: &Theme, chats: Rect) {
        let key = match self.home_list.as_str() {
            CLOUD_TAB => ui.scope_builder(UiBuilder::new().max_rect(chats), |ui| cloud_list_view(ui, t, cloud::Provider::OWN)).inner,
            id => match plugin_tab(id) {
                Some(tab) => {
                    ui.scope_builder(UiBuilder::new().max_rect(chats), |ui| tab.show(ui, self));
                    None
                }
                // (Also a tab whose plugin is gone: the conversations.)
                None => {
                    self.local_list(ui, t, chats);
                    None
                }
            },
        };
        if let Some(key) = key {
            self.open(key);
        }
    }

    /// This machine's conversations, in `area`: the search field over the
    /// list once there is something to search, and the list.
    fn local_list(&mut self, ui: &mut Ui, t: &Theme, mut area: Rect) {
        let rows = self.chat_rows();
        let mut enter = false;
        if !rows.is_empty() || !self.chat_filter.is_empty() {
            let field = Rect::from_min_max(egui::pos2(area.left() + 8.0, area.top() + 8.0), egui::pos2(area.right() - BAR_ROOM - 8.0, area.top() + 8.0 + SEARCH_H));
            enter = search_field(ui, t, field, &mut self.chat_filter);
            area.min.y = field.bottom() + 4.0;
        }
        let rows = filter_rows(rows, &self.chat_filter);
        let pick = if rows.is_empty() && !self.chat_filter.trim().is_empty() {
            ui.painter().text(area.center(), Align2::CENTER_CENTER, "No matching conversations", FontId::proportional(12.5), t.ink3);
            ChatListAction::default()
        } else {
            ui.scope_builder(UiBuilder::new().max_rect(area), |ui| chat_list(ui, t, &rows, true)).inner
        };
        // Enter in the search field opens the first match.
        if enter
            && !self.chat_filter.trim().is_empty()
            && let Some(first) = rows.first()
        {
            self.open(first.id.clone());
        }
        if let Some(id) = pick.rename {
            self.start_rename(&id);
        }
        if let Some(id) = pick.delete {
            self.deleting_chat = Some(id);
        }
        if let Some(id) = pick.clear_scratch {
            std::thread::spawn(move || crate::scratch::clear_conversation(&id));
        }
        if let Some(id) = pick.open {
            self.open(id);
        }
    }

    /// Asks before deleting a conversation, which can't be undone.
    fn delete_chat_dialog(&mut self, ctx: &egui::Context, t: &Theme) {
        let Some(id) = self.deleting_chat.clone() else { return };
        let Some(s) = self.sessions.get(&id) else {
            self.deleting_chat = None;
            return;
        };
        let title = s.conv.lock().unwrap().title.clone();
        let title = if title.is_empty() { "Untitled".to_string() } else { title };
        let body = |ui: &mut Ui| {
            ui.label(RichText::new("The conversation, its side chat, its agents and its transcript files go. This can't be undone.").color(t.ink2));
        };
        let (delete, close) = crate::widgets::confirm_dialog(ctx, t, "chat-delete", &format!("Delete {title}?"), body, None, "Delete", false);
        if delete {
            self.delete_conversation(&id);
        }
        if delete || close {
            self.deleting_chat = None;
        }
    }

    /// The dialog about an account's usage-limit reset, the only place
    /// one is used (`resets_view`).
    fn reset_dialog(&mut self, ctx: &egui::Context, t: &Theme) {
        let Some(d) = &mut self.reset_dialog else { return };
        let (open, used) = d.show(ctx, t);
        if !open {
            self.reset_dialog = None;
            if used {
                self.account.lock().unwrap().fetched = None; // its usage changed: fetch it again now
            }
        }
    }

    /// The conversations, the latest first.
    fn chat_rows(&self) -> Vec<ChatRow> {
        let mut rows: Vec<ChatRow> = self
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                let c = s.conv.lock().unwrap();
                if c.side_chat_of.is_some() { return None; }
                let folder = c.folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                Some(ChatRow {
                    id: id.clone(),
                    title: if c.title.is_empty() { "Untitled".into() } else { c.title.clone() },
                    // A subagent says so, and which session started it.
                    folder: match c.parent.as_deref().and_then(|p| self.sessions.get(p)) {
                        Some(p) => {
                            let title = p.conv.lock().unwrap().title.clone();
                            format!("{folder}, agent {} of {}", c.agent_id, if title.is_empty() { "Untitled" } else { &title })
                        }
                        _ => folder,
                    },
                    at: c.last_message_at,
                    busy: s.live.lock().unwrap().busy,
                    paused: c.paused,
                    renamable: c.parent.is_none(),
                })
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.at));
        rows
    }

    /// The providers with accounts, each a box with their accounts summed;
    /// a click opens its accounts below it (a right-click on one for what
    /// can be done with it: no picking, conversations keep to theirs, see
    /// `crate::rotation`). Pinned under them, the button to add another
    /// (or the form adding one), the list fading out above it.
    fn account_column(&mut self, ui: &mut Ui, t: &Theme) {
        // A removal or move done in the background.
        if let Some(rx) = &self.account_job
            && let Ok(r) = rx.try_recv()
        {
            self.account_job = None;
            self.account_note = r.err();
            self.signed_in = still_signed_in();
            if self.signed_in {
                self.fetch_models(ui.ctx());
            }
            self.account.lock().unwrap().fetched = None; // fetch again now
        }
        let area = ui.max_rect();
        let foot_h = if self.adding_form_open() { crate::composer::COMPOSER_H } else { 40.0 };
        let list = Rect::from_min_max(area.min, egui::pos2(area.right(), area.bottom() - foot_h - HOME_GAP));
        let foot = Rect::from_min_max(egui::pos2(area.left(), area.bottom() - foot_h), egui::pos2(area.right() - 8.0, area.bottom()));
        let mut act = None;
        let mut reset = None;
        let busy = self.account_job.is_some();
        ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
            ScrollArea::vertical().id_salt("home-accounts").auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = HOME_GAP;
                // The boxes stop short of the right edge, where the scroll bar goes.
                let w = ui.available_width() - 8.0;
                ui.set_max_width(w);
                // What plugins say first (a connection's state).
                for h in &crate::plugins::parts().hooks {
                    h.providers_top(ui, self);
                }
                if let Some(e) = &self.account_note {
                    crate::widgets::error_note(ui, t, e);
                }
                // Logins kept on this machine though meant for the vault.
                for note in [codex::note(), openrouter::KEYS.note(), crate::nodeclub::KEYS.note(), freebuff::note()].into_iter().flatten() {
                    caption(ui, &note, t.ink3);
                }
                if busy {
                    crate::widgets::working(ui, t, "Updating accounts…");
                }
                for p in Provider::ALL {
                    let n = providers_view::account_count(p);
                    if n == 0 {
                        continue;
                    }
                    let open = self.providers_open.contains(&p);
                    if providers_view::provider_box(ui, t, p, n, &providers_view::shown(p), open, w).clicked() {
                        if open {
                            self.providers_open.retain(|&o| o != p);
                        } else {
                            self.providers_open.push(p);
                        }
                    }
                    if open {
                        // Its accounts: darker, a little narrower, close
                        // under it, as a drop-down.
                        ui.add_space(4.0 - HOME_GAP);
                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            account_cards(ui, t, p, w, busy, &mut act, &mut reset);
                        });
                    }
                }
                // Room for the fade at the end.
                ui.add_space(8.0);
            });
        });
        providers_view::fade(ui.painter(), Rect::from_min_max(egui::pos2(list.left(), list.bottom() - 22.0), egui::pos2(list.right() - 8.0, list.bottom() + 1.0)), t.panel);
        ui.scope_builder(UiBuilder::new().max_rect(foot), |ui| self.add_account_box(ui, t));
        if let Some((id, grant)) = reset
            && let Some(a) = api::accounts().into_iter().find(|a| a.id == id)
        {
            self.reset_dialog = Some(crate::resets_view::ResetDialog::open(ui.ctx(), &a, &grant));
        }
        // Removals and moves can wait on a vault: they run in the
        // background, and the column says when they are done.
        if let Some(act) = act {
            let (tx, rx) = std::sync::mpsc::channel();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let _ = tx.send(run_account_act(act));
                ctx.request_repaint();
            });
            self.account_note = None;
            self.account_job = Some(rx);
        }
    }

    /// A form adding an account is open (it takes the place of the button).
    fn adding_form_open(&self) -> bool {
        self.codex_login.is_some()
            || self.adding_local
            || self.adding_openrouter.open
            || self.adding_nodeclub.open
            || self.adding.is_some()
    }

    /// "+ Add account", which asks what to add: a Claude account, a Codex
    /// account, an API key (OpenRouter, NodeClub), a Freebuff account or a
    /// local server. While one is being added, its box.
    fn add_account_box(&mut self, ui: &mut Ui, t: &Theme) {
        let w = ui.available_width();
        if self.adding_form(ui, t, w) {
            return;
        }
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 40.0), Sense::click());
        let hovered = resp.hovered();
        let p = ui.painter();
        p.rect_filled(rect, 0.0, if hovered { t.panel2 } else { t.panel });
        p.rect_stroke(rect, 0.0, Stroke::new(1.0, if hovered { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
        p.text(rect.center(), Align2::CENTER_CENTER, "+  Add account", FontId::proportional(13.0), if hovered { t.ink } else { t.ink3 });
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let pick = egui::Popup::menu(&resp).width(w).show(add_menu).and_then(|r| r.inner);
        self.account_note = if pick.is_some() { None } else { self.account_note.take() };
        if let Some(pick) = pick {
            self.start_adding(ui.ctx(), pick);
        }
    }

    /// The box of what is being added, if anything: a form adding a Codex
    /// login, a local server or a key, or a Claude login's code. True when
    /// one showed.
    fn adding_form(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        // True once added.
        let added = if self.codex_login.is_some() {
            Some(self.codex_add_box(ui, t, w))
        } else if self.adding_local {
            Some(self.local_add_box(ui, t, w))
        } else if self.adding_openrouter.open {
            Some(self.openrouter_add_box(ui, t, w))
        } else if self.adding_nodeclub.open {
            Some(self.nodeclub_add_box(ui, t, w))
        } else {
            None
        };
        if let Some(added) = added {
            if added {
                self.accounts_changed(ui.ctx());
            }
            return true;
        }
        let Some(add) = &mut self.adding else { return false };
        let (done, cancel) = add_box(ui, t, add, w);
        if cancel {
            self.adding = None;
        } else if done && !add.code.trim().is_empty() {
            match api::finish_login(&add.login, &add.code) {
                Ok(()) => {
                    self.adding = None;
                    self.account.lock().unwrap().fetched = None; // fetch its usage now
                }
                Err(e) => add.error = Some(e),
            }
        }
        true
    }

    /// Starts adding what was picked from "+ Add account".
    fn start_adding(&mut self, ctx: &egui::Context, pick: Add) {
        match pick {
            Add::Claude => {
                let login = api::start_login();
                let _ = open::that(&login.url);
                self.adding = Some(AddAccount { login, code: String::new(), error: None });
            }
            Add::Codex => {
                let login = codex::start_login();
                let _ = open::that(&login.url);
                self.codex_login = Some(login);
            }
            Add::OpenRouter => self.adding_openrouter.open = true,
            Add::NodeClub => self.adding_nodeclub.open = true,
            Add::Local => self.adding_local = true,
            Add::Freebuff => match freebuff::login_start() {
                Ok(url) => {
                    let _ = open::that(&url);
                    // Keep polling until the browser approves it.
                    let ctx = ctx.clone();
                    std::thread::spawn(move || {
                        for _ in 0..150 {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            if freebuff::login_poll() {
                                freebuff::refresh_session();
                                ctx.request_repaint();
                                break;
                            }
                        }
                    });
                }
                Err(e) => self.account_note = Some(e),
            },
        }
    }
}

/// What "+ Add account" can add.
#[derive(Clone, Copy)]
enum Add {
    Claude,
    Codex,
    OpenRouter,
    NodeClub,
    Freebuff,
    Local,
}

/// "+ Add account"'s menu: a Claude account, then the other providers.
/// Gives the one picked.
fn add_menu(ui: &mut Ui) -> Option<Add> {
    let mut pick = None;
    // With a vault that takes new logins, they go there.
    let shared = api::signing_in_remotely();
    let tip = if shared { format!("Adds a Claude account to {}, for every machine connected to it", api::vault_name()) } else { "Claude subscription, signed in in the browser".to_string() };
    let ctx = ui.ctx().clone();
    let item = |ui: &mut Ui, p: Provider, label: &str| ui.add(Button::image_and_text(providers_view::logo_image(&ctx, p, 14.0), label));
    if item(ui, Provider::Claude, "Claude account").on_hover_text(tip).clicked() {
        pick = Some(Add::Claude);
    }
    let remote = if shared { format!(" Shared through {}.", api::vault_name()) } else { String::new() };
    for (provider, label, tip, add) in [
        (Provider::Codex, "Codex account", "ChatGPT plan, Codex models.", Add::Codex),
        (Provider::OpenRouter, "OpenRouter API key", "OpenRouter, pay per token.", Add::OpenRouter),
        (Provider::NodeClub, "NodeClub API key", "NodeClub, OpenAI-compatible API (Qwen).", Add::NodeClub),
        (Provider::Freebuff, "Freebuff account", "Freebuff, sign in in the browser; its models run a separate system.", Add::Freebuff),
    ] {
        if item(ui, provider, label).on_hover_text(format!("{tip}{remote}")).clicked() {
            pick = Some(add);
        }
    }
    let local = ui.add_enabled(!local::on(), Button::image_and_text(providers_view::logo_image(&ctx, Provider::Local, 14.0), "Local llama.cpp server"));
    if local.on_disabled_hover_text("Already added").clicked() {
        pick = Some(Add::Local);
    }
    if pick.is_some() {
        ui.close();
    }
    pick
}

/// When the accounts' usage was last fetched (in the background).
#[derive(Clone, Default)]
pub(crate) struct AccountState {
    pub(crate) fetched: Option<Instant>,
    pub(crate) loading: bool,
    /// `api::rotations()` then: a rotation fetches usage again.
    pub(crate) rotations: u64,
    /// Whether there were an OpenRouter key and a Codex login when the
    /// models were last listed: a sync with the vault that
    /// brings or takes one lists them again.
    pub(crate) services: Option<(bool, bool, bool, bool, bool)>,
    /// Whether a Claude account was there last time: the model list is
    /// asked for again when one arrives.
    pub(crate) claude: Option<bool>,
}

/// Home's box, width over height (from the concept drawing).
pub(crate) const HOME_RATIO: f32 = 591.0 / 272.0;
/// Padding inside the box's cells, and between account boxes.
pub(crate) const HOME_GAP: f32 = 12.0;
/// The button under the box (Settings, or Home on Settings).
const FOOT_H: f32 = 28.0;
/// The usage preview under Home's accounts.
const PREVIEW_H: f32 = 124.0;

/// Home's box: the box, its two cells (chats left, accounts right, as wide
/// as an account box), their headings, the inner areas the lists go in, and
/// the button under the box.
pub(crate) struct HomeGrid {
    pub(crate) card: Rect,
    /// Where the line between the cells runs.
    pub(crate) split_x: f32,
    pub(crate) chats_head: Rect,
    pub(crate) accounts_head: Rect,
    pub(crate) chats: Rect,
    pub(crate) accounts: Rect,
    /// The usage preview under the accounts, when the box is tall enough.
    preview: Option<Rect>,
}

pub(crate) fn home_grid(area: Rect) -> HomeGrid {
    let room = area.height() - FOOT_H - 8.0;
    let w = (area.width() * 0.86).min(room * 0.86 * HOME_RATIO).max(460.0f32.min(area.width() - 16.0));
    let h = w / HOME_RATIO;
    // The box and the button under it are centred together.
    let top = area.center().y - (h + 8.0 + FOOT_H) / 2.0;
    let card = Rect::from_center_size(egui::pos2(area.center().x, top + h / 2.0), vec2(w, h));
    // An account box's width when there is room; in a small window the
    // chats keep the larger share.
    let acc_w = (w * 0.24).max(BOX_W + 2.0 * HOME_GAP + BAR_ROOM).min(w * 0.45);
    let split_x = card.right() - acc_w;
    let head = |l: f32, r: f32| Rect::from_min_max(egui::pos2(l, card.top()), egui::pos2(r, card.top() + HOME_HEAD));
    let (chats_head, accounts_head) = (head(card.left(), split_x), head(split_x, card.right()));
    let body = |h: Rect| Rect::from_min_max(egui::pos2(h.left() + HOME_GAP, h.bottom() + HOME_GAP), egui::pos2(h.right() - 4.0, card.bottom() - 4.0));
    let chats = Rect::from_min_max(egui::pos2(chats_head.left() + 4.0, chats_head.bottom() + 1.0), egui::pos2(split_x - 4.0, card.bottom() - 1.0));
    let mut accounts = body(accounts_head);
    // The usage preview takes the bottom of the accounts' cell, as wide
    // as an account box, when that leaves room for a couple of accounts.
    let preview = (accounts.height() >= PREVIEW_H + HOME_GAP + 200.0).then(|| {
        let r = Rect::from_min_max(egui::pos2(accounts.left(), card.bottom() - HOME_GAP - PREVIEW_H), egui::pos2(accounts.right() - 8.0, card.bottom() - HOME_GAP));
        accounts.max.y = r.top() - HOME_GAP;
        r
    });
    HomeGrid { card, split_x, chats_head, accounts_head, chats, accounts, preview }
}

/// The solid box, its heading strip and, `split`, the line between the
/// chats and the accounts (Settings has one cell).
fn paint_grid(p: &egui::Painter, t: &Theme, g: &HomeGrid, split: bool) {
    p.rect_filled(g.card, 0.0, t.panel);
    p.rect_stroke(g.card, 0.0, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
    p.hline(g.card.x_range(), g.chats_head.bottom() - 0.5, Stroke::new(1.0, t.line));
    if split {
        p.vline(g.split_x + 0.5, g.card.y_range(), Stroke::new(1.0, t.line));
    }
}

/// A cell's heading, small and quiet.
fn cell_heading(p: &egui::Painter, t: &Theme, rect: Rect, text: &str) {
    p.text(egui::pos2(rect.left() + 16.0, rect.center().y), Align2::LEFT_CENTER, text, FontId::proportional(12.5), t.ink2);
}

/// Signing in another account: the code from the sign-in page goes in
/// here. Returns (Add pressed, Cancel pressed).
pub(crate) fn add_box(ui: &mut Ui, t: &Theme, add: &mut AddAccount, w: f32) -> (bool, bool) {
    side_box(ui, t, w, |ui| {
        sign_in_heading(ui, t, "Add account", &add.login.url);
        let edit = TextEdit::singleline(&mut add.code).id(Id::unique("add-account-code")).hint_text("Authorization code");
        add_field(ui, t, edit, None, add.error.as_ref(), |ui| {
            caption(ui, "Waiting for sign-in…", t.ink3);
        })
    })
    .inner
}

/// What a right-click on an account asked for: to remove it (or sign it
/// out), or to move logins on this machine to the vault.
enum AccountAct {
    Remove(Provider, String),
    Move(Provider, Vec<String>),
}

/// Does `act` (off the UI thread: it may wait on a vault).
fn run_account_act(act: AccountAct) -> Result<(), String> {
    match act {
        AccountAct::Remove(Provider::Claude, id) => {
            api::remove_account(id.parse().map_err(|_| "No such account.".to_string())?);
            Ok(())
        }
        AccountAct::Remove(Provider::Codex, id) => codex::sign_out(&id),
        AccountAct::Remove(Provider::OpenRouter, id) => openrouter::KEYS.remove(&id),
        AccountAct::Remove(Provider::NodeClub, id) => crate::nodeclub::KEYS.remove(&id),
        AccountAct::Remove(Provider::Freebuff, id) => freebuff::sign_out(&id),
        AccountAct::Remove(Provider::Local, _) => {
            local::remove();
            Ok(())
        }
        AccountAct::Move(p, ids) => ids.iter().try_for_each(|id| match p {
            Provider::Claude => api::move_to_vault(id.parse().map_err(|_| "No such account.".to_string())?),
            Provider::Codex => codex::move_to_vault(id),
            Provider::OpenRouter => openrouter::KEYS.move_to_vault(id),
            Provider::NodeClub => crate::nodeclub::KEYS.move_to_vault(id),
            Provider::Freebuff => freebuff::move_to_vault(id),
            Provider::Local => Ok(()),
        }),
    }
}

/// Any login or key is left (or a vault, which has them).
fn still_signed_in() -> bool {
    Provider::ALL.iter().any(|p| p.on())
}

/// One account under its provider's box: `panel` draws it (darker, `w`
/// less a margin each side); a right-click offers moving it to the vault (`movable`) and `remove`.
#[allow(clippy::too_many_arguments)]
fn account_card(
    ui: &mut Ui,
    t: &Theme,
    w: f32,
    id: (Provider, &str),
    movable: bool,
    remove: &str,
    busy: bool,
    act: &mut Option<AccountAct>,
    panel: impl FnOnce(&mut Ui, &Theme, f32) -> egui::Response,
) {
    let mut dark = t.clone();
    dark.panel = crate::theme::mix(t.panel, t.bg, 0.65);
    let r = ui.horizontal(|ui| {
        ui.add_space(8.0);
        panel(ui, &dark, w - 16.0)
    });
    let resp = ui.interact(r.inner.rect, Id::unique(("account-card", id.0.id(), id.1)), Sense::click());
    // (The right-click menu is the only way to move or remove an account.)
    let resp = resp.on_hover_text(if movable { "On this machine. Right-click to move or remove it." } else { "Right-click for options" });
    if busy {
        return;
    }
    // The menu stays only while its card stays put: Esc, a scroll, or the
    // card moving (the column scrolled, a box opened above) closes it, so
    // it can never sit beside another account.
    let menu = egui::Popup::default_response_id(&resp);
    let at = menu.with("card-at");
    if egui::Popup::is_id_open(ui.ctx(), menu) {
        let opened_at = ui.ctx().data(|d| d.get_temp::<Rect>(at));
        let (esc, scrolled) = ui.input(|i| (i.key_pressed(egui::Key::Escape), i.raw.events.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. })) || i.smooth_scroll_delta != egui::Vec2::ZERO));
        if esc || scrolled || opened_at.is_some_and(|r| r != resp.rect) {
            egui::Popup::close_id(ui.ctx(), menu);
            ui.ctx().data_mut(|d| d.remove::<Rect>(at));
            return;
        }
        if opened_at.is_none() {
            ui.ctx().data_mut(|d| d.insert_temp(at, resp.rect));
        }
    } else {
        ui.ctx().data_mut(|d| d.remove::<Rect>(at));
    }
    resp.context_menu(|ui| {
        if movable && ui.button(format!("Move to {}", api::vault_name())).clicked() {
            *act = Some(AccountAct::Move(id.0, vec![id.1.to_string()]));
            ui.close();
        }
        if ui.button(remove).clicked() {
            *act = Some(AccountAct::Remove(id.0, id.1.to_string()));
            ui.close();
        }
    });
}

/// Provider `p`'s accounts, under its box. Each says how many conversations
/// are on it now (`crate::rotation`); under a Claude account, the
/// usage-limit resets it is offered (`reset`: the one clicked).
fn account_cards(ui: &mut Ui, t: &Theme, p: Provider, w: f32, busy: bool, act: &mut Option<AccountAct>, reset: &mut Option<(u64, String)>) {
    let active = crate::rotation::active(p, api::now_ms());
    let on = |id: &str| active.get(id).map(|n| if *n == 1 { "1 active".to_string() } else { format!("{n} active") });
    // How many conversations are on it, or that it is on this machine only.
    let status = |id: &str, movable: bool| on(id).or_else(|| movable.then(|| "this machine".to_string()));
    let paired = api::vault_on();
    match p {
        Provider::Claude => {
            let accounts = api::accounts();
            let (main, fallback): (Vec<_>, Vec<_>) = accounts.iter().partition(|a| !a.fallback);
            let remove = if paired { "Remove shared account (every machine)" } else { "Sign out of this account" };
            for a in &main {
                let id = a.id.to_string();
                let status = on(&id);
                account_card(ui, t, w, (p, &id), false, remove, busy, act, |ui, t, w| usage_panel(ui, t, Some(a), status.as_deref(), false, w));
                if let Some(g) = crate::resets_view::rows(ui, t, a, busy) {
                    *reset = Some((a.id, g));
                }
            }
            if !fallback.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    caption(ui, "Local fallback accounts", t.ink3).on_hover_text(format!("Accounts on this machine, used when those of {} are unavailable.", api::vault_name()));
                });
                for a in &fallback {
                    let id = a.id.to_string();
                    let status = status(&id, a.movable);
                    account_card(ui, t, w, (p, &id), a.movable, "Remove from this machine", busy, act, |ui, t, w| usage_panel(ui, t, Some(a), status.as_deref(), false, w));
                    if let Some(g) = crate::resets_view::rows(ui, t, a, busy) {
                        *reset = Some((a.id, g));
                    }
                }
                let movable: Vec<String> = fallback.iter().filter(|a| a.movable).map(|a| a.id.to_string()).collect();
                if movable.len() > 1 && !busy {
                    let r = ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.button(RichText::new(format!("Move all {} to {}", movable.len(), api::vault_name())).small())
                    });
                    if r.inner.on_hover_text(format!("Transfers logins and token renewal to {}; only access tokens remain locally.", api::vault_name())).clicked() {
                        *act = Some(AccountAct::Move(p, movable));
                    }
                }
            }
        }
        Provider::Codex => {
            let remove = if paired { "Sign out (every machine)" } else { "Sign out" };
            for (id, a) in codex::accounts() {
                let movable = codex::movable(&id);
                let status = status(&id, movable);
                account_card(ui, t, w, (p, &id), movable, remove, busy, act, |ui, t, w| usage_panel(ui, t, Some(&a), status.as_deref(), false, w));
            }
        }
        Provider::OpenRouter => {
            for a in openrouter::accounts() {
                let movable = openrouter::KEYS.movable(&a.id);
                let status = status(&a.id, movable);
                let remove = format!("Remove OpenRouter key {}{}", a.label, if a.remote { " (all clients)" } else { "" });
                account_card(ui, t, w, (p, &a.id), movable, &remove, busy, act, |ui, t, w| openrouter_panel(ui, t, Some(&a), ("Spent", a.spent), status.as_deref(), w));
            }
        }
        Provider::NodeClub => {
            for a in crate::nodeclub::accounts() {
                let movable = crate::nodeclub::KEYS.movable(&a.id);
                let status = status(&a.id, movable);
                let remove = format!("Remove NodeClub key {}{}", a.label, if a.remote { " (all clients)" } else { "" });
                account_card(ui, t, w, (p, &a.id), movable, &remove, busy, act, |ui, t, w| nodeclub_panel(ui, t, Some(&a), status.as_deref(), w));
            }
        }
        Provider::Freebuff => {
            for a in freebuff::accounts() {
                let movable = freebuff::movable(&a.id);
                let status = status(&a.id, movable);
                let remove = if a.remote { "Sign out (all clients)" } else { "Sign out" };
                account_card(ui, t, w, (p, &a.id), movable, remove, busy, act, |ui, t, w| crate::composer::freebuff_panel(ui, t, Some(&a), status.as_deref(), w));
            }
        }
        Provider::Local => {
            account_card(ui, t, w, (p, "local"), false, "Remove server", busy, act, crate::composer::local_panel);
        }
    }
}

/// Another account being signed in from Home.
pub(crate) struct AddAccount {
    pub(crate) login: api::Login,
    pub(crate) code: String,
    pub(crate) error: Option<String>,
}
/// A picture built into OAK, as a texture (pixel art: no smoothing).
/// The forests are placeholders, made by assets/placeholder-forest.py.
pub(crate) fn picture(ctx: &egui::Context, cache: &mut HashMap<&'static str, egui::TextureHandle>, name: &'static str) -> egui::TextureHandle {
    cache
        .entry(name)
        .or_insert_with(|| {
            let bytes: &[u8] = match name {
                "forest-header" => include_bytes!("../assets/forest-header.png"),
                _ => include_bytes!("../assets/forest-home.png"),
            };
            let img = image::load_from_memory(bytes).expect("built-in picture decodes").to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            ctx.load_texture(name, egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), egui::TextureOptions::NEAREST)
        })
        .clone()
}

/// Paints `tex` over `rect` like CSS `background-size: cover`: filling it,
/// centred, cropping whichever side is too long.
pub(crate) fn paint_cover(p: &egui::Painter, rect: Rect, tex: &egui::TextureHandle, tint: Color32) {
    let [w, h] = tex.size().map(|v| v as f32);
    let (ra, ia) = (rect.width() / rect.height(), w / h);
    let uv = if ra > ia {
        let v = ia / ra;
        Rect::from_min_max(egui::pos2(0.0, (1.0 - v) / 2.0), egui::pos2(1.0, (1.0 + v) / 2.0))
    } else {
        let u = ra / ia;
        Rect::from_min_max(egui::pos2((1.0 - u) / 2.0, 0.0), egui::pos2((1.0 + u) / 2.0, 1.0))
    };
    p.image(tex.id(), rect, uv, tint);
}

/// A conversation in Home's list.
pub(crate) struct ChatRow {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) folder: String,
    pub(crate) at: i64,
    pub(crate) busy: bool,
    pub(crate) paused: bool,
    /// It can be renamed (not a subagent).
    pub(crate) renamable: bool,
}

/// What a click or right-click in the list asked for.
#[derive(Default)]
pub(crate) struct ChatListAction {
    /// The conversation clicked (opened).
    pub(crate) open: Option<String>,
    /// The conversation whose "Rename…" was picked.
    pub(crate) rename: Option<String>,
    /// The conversation whose "Delete conversation" was picked.
    pub(crate) delete: Option<String>,
    /// The conversation whose "Clear scratchpad" was picked.
    pub(crate) clear_scratch: Option<String>,
}

/// The Chats list, after claude.dev's post list: the title with the folder
/// under it, and on the right when it last changed with the date
/// (mm/dd/yy) under that; a faint line between rows, and a soft highlight
/// under the pointer. A dot before the title shows OAK working (hollow:
/// paused). The rows stop short of the scroll bar. A click opens a
/// conversation; with `can_delete`, a right-click offers to rename it, clear
/// its scratchpad or delete it.
pub(crate) fn chat_list(ui: &mut Ui, t: &Theme, rows: &[ChatRow], can_delete: bool) -> ChatListAction {
    let mut action = ChatListAction::default();
    list_rows(ui, t, "home-chats", rows.len(), "No conversations", |ui, i, rect, resp| {
        let row = &rows[i];
        let p = ui.painter();
        let y = rect.center().y;
        let (mut x, right) = (rect.left() + 12.0, rect.right() - 12.0);
        if row.busy || row.paused {
            state_dot(p, t, egui::pos2(x + 3.0, y - 9.0), row.paused);
            x += 12.0;
        }
        let big = FontId::proportional(14.5);
        let title = clip_width(ui, &row.title, &big, right - 90.0 - x);
        p.text(egui::pos2(x, y - 9.0), Align2::LEFT_CENTER, title, big, row_title_color(t, resp.hovered()));
        p.text(egui::pos2(x, y + 11.0), Align2::LEFT_CENTER, &row.folder, FontId::proportional(11.5), t.ink3);
        p.text(egui::pos2(right, y - 9.0), Align2::RIGHT_CENTER, ago(row.at), FontId::proportional(12.5), t.ink3);
        p.text(egui::pos2(right, y + 11.0), Align2::RIGHT_CENTER, mdy(row.at), FontId::proportional(11.5), t.ink4);
        if can_delete {
            resp.context_menu(|ui| {
                if row.renamable && ui.button("Rename…").clicked() {
                    action.rename = Some(row.id.clone());
                    ui.close();
                }
                let size = crate::scratch::scan(Duration::from_secs(15), ui.ctx()).of(&row.id);
                let label = match size {
                    Some(n) if n > 0 => format!("Clear scratchpad ({})", crate::scratch::fmt_bytes(n)),
                    _ => "Clear scratchpad".to_string(),
                };
                let clear = ui.add_enabled(size.is_some_and(|n| n > 0), Button::new(label));
                if clear.on_hover_text("Deletes the files in the session's .oak/tmp folder.").on_disabled_hover_text("Its scratchpad is empty.").clicked() {
                    action.clear_scratch = Some(row.id.clone());
                    ui.close();
                }
                if ui.button("Delete conversation").on_hover_text("Removes the conversation and its transcript file.").clicked() {
                    action.delete = Some(row.id.clone());
                    ui.close();
                }
            });
        }
        if resp.clicked() {
            action.open = Some(row.id.clone());
        }
    });
    action
}

/// The search field's height over Home's conversations.
const SEARCH_H: f32 = 28.0;
/// Nerd Font: a magnifying glass.
const SEARCH_ICON: &str = "\u{f002}";

/// Home's conversation search in `rect`: a magnifying glass, the query,
/// and an X clearing it. Ctrl+F focuses it, Esc clears it. True when
/// Enter was pressed in it.
fn search_field(ui: &mut Ui, t: &Theme, rect: Rect, query: &mut String) -> bool {
    let id = Id::unique("home-chat-search");
    if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
        ui.memory_mut(|m| m.request_focus(id));
    }
    let clear = Rect::from_center_size(egui::pos2(rect.right() - 13.0, rect.center().y), vec2(18.0, 18.0));
    let edit = TextEdit::singleline(query)
        .id(id)
        .font(FontId::proportional(12.5))
        .hint_text("Search conversations")
        .margin(Margin { left: 26, right: 24, top: 6, bottom: 6 })
        .desired_width(rect.width());
    let resp = crate::text_menu::put(ui, rect, edit);
    ui.painter().text(egui::pos2(rect.left() + 14.0, rect.center().y), Align2::CENTER_CENTER, SEARCH_ICON, markdown::icon_font(11.0), t.ink3);
    if crate::widgets::left_with(ui, &resp, Key::Escape) {
        query.clear();
    }
    if !query.is_empty() {
        let x = ui.interact(clear, id.with("clear"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Clear search");
        if x.hovered() {
            ui.painter().rect_filled(clear, 3.0, t.tool_hover);
        }
        crate::widgets::draw_x(ui.painter(), clear, if x.hovered() { t.ink } else { t.ink3 });
        if x.clicked() {
            query.clear();
            ui.memory_mut(|m| m.request_focus(id));
        }
    }
    crate::widgets::left_with(ui, &resp, Key::Enter)
}

/// The rows whose title or folder has every word of `query` in it, in
/// any case; all of them for an empty query. The order is kept.
pub(crate) fn filter_rows(rows: Vec<ChatRow>, query: &str) -> Vec<ChatRow> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|r| {
            let (title, folder) = (r.title.to_lowercase(), r.folder.to_lowercase());
            words.iter().all(|w| title.contains(w.as_str()) || folder.contains(w.as_str()))
        })
        .collect()
}

/// Sessions for testing the Home list: title, folder, and minutes since
/// it last changed.
#[cfg(test)]
pub(crate) const DUMMY_CHATS: [(&str, &str, i64); 17] = [
    ("Build a landing page for the forest theme", "oak-site", 42),
    ("Review the pull request for the auth refactor", "backend", 180),
    ("Profile the slow startup of the desktop app", "oak", 410),
    ("Triage the open issues and write a fix list", "oak", 1_300),
    ("Scrape the docs site into markdown", "docs-mirror", 1_900),
    ("Tidy up the settings page code", "oak", 2_900),
    ("Retry logic for rate-limited requests", "api-client", 4_200),
    ("Draft the README for the plugin template", "plugin-template", 5_800),
    ("Write a sudoku solver with backtracking", "scratch", 7_400),
    ("Track API spending per conversation", "oak", 9_900),
    ("Benchmark FETCH against curl", "oak", 12_500),
    ("Map out the codebase for a new contributor", "game-engine", 16_000),
    ("Hook up email notifications for finished tasks", "notifier", 21_000),
    ("Set up the Rust toolchain on the new laptop", "dotfiles", 27_000),
    ("Plan the migration to the new storage format", "backend", 33_000),
    ("Read through the egui changelog for breaking changes", "oak", 39_000),
    ("Release v0.2 and write the changelog", "oak", 46_000),
];
/// Home's tab of this machine's conversations (`App::home_list`; also
/// what an empty or unknown one shows).
pub(crate) const LOCAL_TAB: &str = "local";
/// Home's tab of Oak's own cloud sessions.
pub(crate) const CLOUD_TAB: &str = "cloud";

/// The plugin tab with this id, while it is there.
fn plugin_tab(id: &str) -> Option<&'static Arc<dyn oak_sdk::HomeTab>> {
    crate::plugins::parts().home_tabs.iter().find(|tab| tab.id() == id && tab.shown())
}

/// A tab in the heading of Home's left cell.
struct HomeTabLook {
    id: String,
    icon: String,
    /// The icon is a character of the text font, not a Nerd Font symbol.
    icon_is_text: bool,
    label: String,
    tip: String,
    order: i32,
}

/// The left cell's tabs, in order: Conversations, Cloud, and the plugins'.
fn home_tab_looks() -> Vec<HomeTabLook> {
    let own = |id: &str, icon: &str, label: &str, tip: &str, order| HomeTabLook { id: id.into(), icon: icon.into(), icon_is_text: false, label: label.into(), tip: tip.into(), order };
    let mut tabs = vec![
        own(LOCAL_TAB, "", "Conversations", "This machine's conversations", 0),
        own(CLOUD_TAB, crate::cloud_view::CLOUD_ICON, "Cloud", "Claude Code sessions in Anthropic's cloud, on your Claude accounts", 10),
    ];
    for tab in crate::plugins::parts().home_tabs.iter().filter(|tab| tab.shown()) {
        let id = tab.id();
        if !id.is_empty() && tabs.iter().all(|x| x.id != id) {
            tabs.push(HomeTabLook { id, icon: tab.icon(), icon_is_text: tab.icon_is_text(), label: tab.label(), tip: tab.tip(), order: tab.order() });
        }
    }
    // (Stable: ties keep the order plugins registered in.)
    tabs.sort_by_key(|tab| tab.order);
    tabs
}

/// The tabs in the heading of Home's left cell (Conversations, Cloud, and
/// those plugins add), each with its icon, the one shown in ink over an
/// accent line. Returns the one clicked.
pub(crate) fn home_tabs(ui: &mut Ui, t: &Theme, head: Rect, on: &str) -> Option<String> {
    ui.scope_builder(UiBuilder::new().max_rect(head), |ui| home_tabs_inner(ui, t, head, on)).inner
}

/// How far in from each end the Home tabs fade when more are off-screen.
const HOME_TAB_FADE: f32 = 24.0;

fn home_tabs_inner(ui: &mut Ui, t: &Theme, head: Rect, on: &str) -> Option<String> {
    let tabs = home_tab_looks();
    let font = FontId::proportional(12.5);
    let icons = markdown::icon_font(12.0);
    // Each tab's width, and how far the row scrolls when they don't all fit.
    let widths: Vec<f32> = tabs
        .iter()
        .map(|tab| 24.0 + if tab.icon.is_empty() { 0.0 } else { 18.0 } + crate::widgets::text_width(ui, &tab.label, &font))
        .collect();
    let room = (head.width() - 4.0).max(0.0);
    let max_scroll = (widths.iter().sum::<f32>() - room).max(0.0);
    // (A tab that is gone, with its plugin: the conversations show.)
    let on_idx = tabs.iter().position(|tab| tab.id == on).unwrap_or(0);

    // The scroll offset, kept across frames; the plain wheel (or the touchpad
    // either way) over the row scrolls it, and the shown tab is brought into
    // view, clear of the faded ends, when it changes or the row resizes.
    let id = ui.scope_id().with("home-tabs-scroll");
    let (mut scroll, last_idx, last_room) = ui.data(|d| d.get_temp::<(f32, usize, f32)>(id)).unwrap_or((0.0, usize::MAX, -1.0));
    if max_scroll > 0.0 && ui.rect_contains_pointer(head) {
        let d = ui.input_mut(|i| std::mem::take(&mut i.smooth_scroll_delta));
        scroll -= d.x + d.y;
    }
    if last_idx != on_idx || last_room != room {
        let left: f32 = widths[..on_idx].iter().sum();
        scroll = scroll.max(left + widths[on_idx] - room + HOME_TAB_FADE).min(left - HOME_TAB_FADE);
    }
    scroll = scroll.clamp(0.0, max_scroll);
    ui.data_mut(|d| d.insert_temp(id, (scroll, on_idx, room)));

    ui.set_clip_rect(head.intersect(ui.clip_rect()));
    let mut x = head.left() + 4.0 - scroll;
    let mut picked = None;
    for (i, (tab, w)) in tabs.iter().zip(&widths).enumerate() {
        let rect = Rect::from_min_max(egui::pos2(x, head.top()), egui::pos2(x + w, head.bottom() - 1.0));
        x += w;
        if !rect.intersects(head) {
            continue;
        }
        // A tab half scrolled out answers only where it shows.
        let mut resp = ui.interact(rect.intersect(head), Id::unique(("home-tab", &tab.id)), Sense::click());
        if !tab.tip.is_empty() {
            resp = resp.on_hover_text(&tab.tip);
        }
        let lit = i == on_idx;
        if lit {
            ui.painter().hline(rect.left() + 6.0..=rect.right() - 6.0, rect.bottom() - 1.0, Stroke::new(2.0, t.accent));
        }
        let color = if lit { t.ink } else if resp.hovered() { t.ink2 } else { t.ink3 };
        let icon_font = if tab.icon_is_text { FontId::proportional(14.0) } else { icons.clone() };
        tab_face(ui, t, rect, resp.hovered() && !lit, (&tab.icon, icon_font, if lit { t.accent } else { color }), &tab.label, color);
        if resp.clicked() && !lit {
            picked = Some(tab.id.clone());
        }
    }
    // Faded ends where there are more tabs to scroll to.
    let ends = head.top()..=head.bottom() - 1.0;
    if scroll > 0.5 {
        crate::widgets::fade(ui.painter(), Rect::from_x_y_ranges(head.left()..=head.left() + HOME_TAB_FADE, ends.clone()), t.panel, true);
    }
    if scroll < max_scroll - 0.5 {
        crate::widgets::fade(ui.painter(), Rect::from_x_y_ranges(head.right() - HOME_TAB_FADE..=head.right(), ends), t.panel, false);
    }
    picked
}

/// Nerd Font: a duck.
pub(crate) const DUCK_ICON: &str = "\u{f01e5}";

/// "Duck.ai" at the right end of the tabs' row, styled as a tab (it opens
/// its own page, so it is never lit). Returns its width and whether it
/// was clicked; in a row too narrow for it and a tab, it isn't drawn.
fn duck_tab(ui: &mut Ui, t: &Theme, head: Rect) -> (f32, bool) {
    let font = FontId::proportional(12.5);
    let icons = markdown::icon_font(13.0);
    let text_w = crate::widgets::text_width(ui, "Duck.ai", &font);
    let w = 24.0 + 18.0 + text_w;
    if head.width() < w + 160.0 {
        return (0.0, false);
    }
    let rect = Rect::from_min_max(egui::pos2(head.right() - 4.0 - w, head.top()), egui::pos2(head.right() - 4.0, head.bottom() - 1.0));
    let resp = ui.interact(rect, Id::unique("home-tab-duck"), Sense::click()).on_hover_text("A private chat on duck.ai, kept nowhere");
    let color = if resp.hovered() { t.ink2 } else { t.ink3 };
    tab_face(ui, t, rect, resp.hovered(), (DUCK_ICON, icons, color), "Duck.ai", color);
    (w + 8.0, resp.clicked())
}

/// A Home tab's face in `rect`: lit by the pointer (`hover`), its icon
/// (none when empty: its font and color) and its label.
fn tab_face(ui: &Ui, t: &Theme, rect: Rect, hover: bool, (icon, icon_font, icon_color): (&str, FontId, Color32), label: &str, color: Color32) {
    let p = ui.painter();
    if hover {
        // Under the box's top border, which the tab's top edge sits on.
        p.rect_filled(Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 1.0), rect.max), 0.0, t.tool_hover);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let mut tx = rect.left() + 12.0;
    if !icon.is_empty() {
        p.text(egui::pos2(tx + 6.0, rect.center().y), Align2::CENTER_CENTER, icon, icon_font, icon_color);
        tx += 18.0;
    }
    p.text(egui::pos2(tx, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(12.5), color);
}

/// Nerd Font: an arrow up in a circle, and a turning arrow.
const UPDATE_ICON: &str = "\u{f0aa}";
const RESTART_ICON: &str = "\u{f01e}";

/// An update, at the right end of `head` (left of Duck.ai): "Update
/// available  0.1.276 → 0.1.280", which downloads and installs it; then its
/// progress; then "Restart to update", which restarts into it. Nothing
/// when Oak is up to date, or when the row is too narrow. Returns the width
/// it took.
fn update_tab(ui: &mut Ui, t: &Theme, head: Rect) -> f32 {
    use crate::update::{self, Phase};
    let s = update::status();
    let current = update::VERSION;
    let (icon, label, tip, lit, clickable) = match &s.phase {
        Phase::None => return 0.0,
        Phase::Available(r) => (
            UPDATE_ICON,
            format!("Update available  {current} → {}", r.version),
            format!("Downloads and installs Oak {}. Restart is manual.{}", r.version, notes_tip(&r.notes)),
            true,
            true,
        ),
        Phase::Downloading(r, f) => (
            UPDATE_ICON,
            if *f >= 0.0 { format!("Downloading {}…  {:.0}%", r.version, f * 100.0) } else { format!("Downloading {}…", r.version) },
            format!("Downloading Oak {}.", r.version),
            false,
            false,
        ),
        Phase::Ready(r) => (
            RESTART_ICON,
            format!("Restart to update  {current} → {}", r.version),
            format!("Oak {} installed. Active conversations pause for restart, then resume.", r.version),
            true,
            true,
        ),
        Phase::Failed(r, e) => (UPDATE_ICON, format!("Update to {} failed", r.version), format!("{e}\n\nRetry update"), false, true),
    };
    let font = FontId::proportional(12.5);
    let icons = markdown::icon_font(12.0);
    let w = 24.0 + 18.0 + crate::widgets::text_width(ui, &label, &font);
    if head.width() < w + 160.0 {
        return 0.0;
    }
    let rect = Rect::from_min_max(egui::pos2(head.right() - w, head.top()), egui::pos2(head.right(), head.bottom() - 1.0));
    let sense = if clickable { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, Id::unique("home-tab-update"), sense).on_hover_text(tip);
    let failed = matches!(s.phase, Phase::Failed(..));
    let color = if failed { t.error } else if lit { t.accent } else { t.ink3 };
    tab_face(ui, t, rect, resp.hovered() && clickable, (icon, icons, color), &label, if failed || lit { color } else { t.ink2 });
    if matches!(s.phase, Phase::Downloading(..)) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
    if resp.clicked() {
        match s.phase {
            Phase::Ready(_) => update::restart(ui.ctx()),
            _ => update::install(ui.ctx()),
        }
    }
    w + 4.0
}

/// The start of a release's notes, for a tooltip.
fn notes_tip(notes: &str) -> String {
    let lines: Vec<&str> = notes.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).take(8).collect();
    if lines.is_empty() {
        return String::new();
    }
    let more = if notes.lines().filter(|l| !l.trim().is_empty()).count() > lines.len() { "\n…" } else { "" };
    format!("\n\n{}{more}", lines.join("\n"))
}

/// Nerd Font: two arrows in a circle.
const REFRESH_ICON: &str = "\u{f021}";

/// Oak's version, against the right end of the Providers heading, with a
/// refresh button left of it that looks for an update now (and says how
/// that went, for a few seconds).
fn version_label(ui: &mut Ui, t: &Theme, head: Rect) {
    use crate::update;
    let font = FontId::proportional(11.5);
    let text = format!("v{}", update::VERSION);
    let w = crate::widgets::text_width(ui, &text, &font);
    let mut room = head.width() - 16.0 - crate::widgets::text_width(ui, "Providers", &FontId::proportional(12.5)) - 16.0;
    if room < w + 12.0 {
        return;
    }
    room -= w + 12.0;
    let s = update::status();
    let p = ui.painter().clone();
    p.text(egui::pos2(head.right() - 12.0, head.center().y), Align2::RIGHT_CENTER, &text, font, t.ink3);
    // The refresh button.
    const BUTTON: f32 = 20.0;
    if room < BUTTON + 4.0 {
        return;
    }
    let right = head.right() - 12.0 - w - 4.0;
    let button = Rect::from_center_size(egui::pos2(right - BUTTON / 2.0, head.center().y), vec2(BUTTON, BUTTON));
    let build = if update::is_release() { "release" } else { "local build" };
    let state = if s.checking {
        "Looking for an update…".to_string()
    } else {
        match (&s.phase, &s.checked) {
            (update::Phase::Available(r) | update::Phase::Ready(r), _) => format!("Oak {} is available.", r.version),
            (_, Some((at, Ok(())))) => format!("Up to date (checked {}).", ago_short(at.elapsed())),
            (_, Some((_, Err(e)))) => format!("Couldn't look for an update: {e}"),
            (_, None) => "Not checked for updates yet.".to_string(),
        }
    };
    let resp = ui
        .interact(button, Id::unique("home-version-refresh"), Sense::click())
        .on_hover_text(format!("Oak {} ({build}, commit {})\n{state}\nCheck for updates", update::VERSION, update::COMMIT));
    if resp.hovered() {
        p.rect_filled(button, 3.0, t.tool_hover);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // The button lit while it looks, then for a few seconds how the look it
    // asked for went: a tick (up to date), a cross (couldn't; the tooltip
    // says why) or an arrow (an update, which appears left of Duck.ai).
    const SHOW: std::time::Duration = std::time::Duration::from_secs(4);
    let result = match (&s.checked, s.asked) {
        (Some((at, r)), Some(asked)) if !s.checking && *at >= asked && at.elapsed() < SHOW => Some(match (r, &s.phase) {
            (Err(_), _) => ("\u{f00d}", t.error),
            (Ok(()), update::Phase::None) => ("\u{f00c}", t.accent),
            (Ok(()), _) => (UPDATE_ICON, t.accent),
        }),
        _ => None,
    };
    let (icon, color) = match result {
        Some(r) => r,
        None if s.checking => (REFRESH_ICON, t.accent),
        None => (REFRESH_ICON, if resp.hovered() { t.ink2 } else { t.ink3 }),
    };
    p.text(button.center(), Align2::CENTER_CENTER, icon, markdown::icon_font(11.5), color);
    if result.is_some() || s.checking {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    if resp.clicked() {
        update::check_soon(ui.ctx());
    }
}

/// "just now", "5 min ago", "2 h ago".
fn ago_short(d: std::time::Duration) -> String {
    match d.as_secs() {
        0..60 => "just now".into(),
        s @ 60..3600 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::app_hover_test::{key, run_frame, test_ctx};

    fn row(title: &str, folder: &str) -> ChatRow {
        ChatRow { id: title.into(), title: title.into(), folder: folder.into(), at: 0, busy: false, paused: false, renamable: true }
    }

    /// Every word must be in the title or the folder, in any case; the
    /// order is kept, and an empty query keeps every row.
    #[test]
    fn filter_rows_matches_words() {
        let rows = || DUMMY_CHATS.iter().map(|(t, f, _)| row(t, f)).collect::<Vec<_>>();
        let titles = |q: &str| filter_rows(rows(), q).into_iter().map(|r| r.title).collect::<Vec<_>>();
        assert_eq!(titles("").len(), DUMMY_CHATS.len());
        assert_eq!(titles("   ").len(), DUMMY_CHATS.len());
        assert_eq!(titles("SUDOKU"), ["Write a sudoku solver with backtracking"]);
        assert_eq!(titles("backend"), ["Review the pull request for the auth refactor", "Plan the migration to the new storage format"]);
        assert_eq!(titles("oak changelog"), ["Read through the egui changelog for breaking changes", "Release v0.2 and write the changelog"]);
        assert!(titles("sudoku backend").is_empty());
    }

    /// Ctrl+F focuses the field, typing fills it, Esc clears it, and Enter
    /// is reported.
    #[test]
    fn search_field_keys() {
        let (ctx, t) = test_ctx(false, false);
        let mut query = String::new();
        let mut time = 0.0;
        let mut run = |query: &mut String, events: Vec<egui::Event>| {
            let mut enter = false;
            run_frame(&ctx, vec2(600.0, 200.0), &mut time, 1.0 / 60.0, events, |ui| enter = search_field(ui, &t, Rect::from_min_size(egui::pos2(10.0, 10.0), vec2(400.0, SEARCH_H)), query));
            enter
        };
        run(&mut query, vec![]);
        run(&mut query, vec![key(Key::F, Modifiers::COMMAND)]);
        run(&mut query, vec![]);
        run(&mut query, vec![egui::Event::Text("sudoku".into())]);
        assert_eq!(query, "sudoku");
        assert!(run(&mut query, vec![key(Key::Enter, Modifiers::NONE)]));
        run(&mut query, vec![key(Key::F, Modifiers::COMMAND)]);
        run(&mut query, vec![]);
        run(&mut query, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(query.is_empty());
    }
}
