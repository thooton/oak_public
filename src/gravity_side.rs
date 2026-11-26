//! Antigravity's sidebar (`gravity.rs`), in place of the tab strip, as the
//! Hub has it: a New conversation pill at the top, Home, Conversations
//! (Home's desktop, its search focused) and Usage under it, then the open tabs as
//! rows (a click shows one, drag to move it, its x or a middle click closes
//! it, a right click has the tab strip's menu), and Settings at the foot.
//! The keys are the tab strip's (`tab_bar_keys`).

use super::*;
use crate::tabs::TabClose;
use crate::theme::mix;
use crate::widgets::{clip_width, draw_x, state_dot};
use egui::Painter;

/// A row's height: the nav's and the tabs'.
const ROW_H: f32 = 32.0;
/// The sidebar's rows' room from its edges.
const PAD: f32 = 10.0;
const HOME_ICON: &str = "\u{f015}";
const LIST_ICON: &str = "\u{f0ca}";
const USAGE_ICON: &str = "\u{f201}";
const PLUS_ICON: &str = "\u{f067}";

/// What a nav row does.
#[derive(Clone, Copy)]
enum Nav {
    Home,
    Conversations,
    Usage,
}

impl App {
    /// The sidebar, in the left panel `ui` is.
    pub(crate) fn side_bar(&mut self, ui: &mut Ui, t: &Theme) {
        self.tab_bar_keys(ui);
        let r = ui.max_rect();
        let p = ui.painter().clone();
        let (left, w) = (r.left() + PAD, r.width() - 2.0 * PAD);
        let mut y = r.top() + 6.0;
        // New conversation.
        let new_on = self.page == Page::Chats && matches!(self.screen, Screen::New);
        let pill = Rect::from_min_size(egui::pos2(left, y), vec2(w, 36.0));
        if pill_button(ui, &p, t, pill, new_on) {
            self.screen = Screen::New;
            self.page = Page::Chats;
            ui.memory_mut(|m| m.request_focus(Id::unique("composer")));
        }
        y = pill.bottom() + 10.0;
        // Conversations are on Home's desktop: Home stays lit there, and
        // Conversations (which focuses their search) is never lit on its
        // own, so the light doesn't jump on the same page.
        for (icon, label, hint, on, nav) in [
            (
                HOME_ICON,
                "Home",
                "Alt+H",
                self.page == Page::Home,
                Nav::Home,
            ),
            (
                LIST_ICON,
                "Conversations",
                "Ctrl+F",
                false,
                Nav::Conversations,
            ),
            (
                USAGE_ICON,
                "Usage",
                "",
                self.page == Page::Usage,
                Nav::Usage,
            ),
        ] {
            let row = Rect::from_min_size(egui::pos2(left, y), vec2(w, ROW_H));
            if nav_row(ui, &p, t, row, icon, label, hint, on) {
                match nav {
                    Nav::Home => {
                        self.page = Page::Home;
                        self.home_menu = false;
                    }
                    Nav::Conversations => {
                        self.page = Page::Home;
                        self.home_menu = false;
                        crate::gravity_home::focus_search(ui.ctx());
                    }
                    Nav::Usage => self.page = Page::Usage,
                }
            }
            y = row.bottom() + 2.0;
        }
        // The open tabs, under their heading, down to Settings.
        y += 14.0;
        let n = self.settings.tabs.len();
        p.text(
            egui::pos2(left + 8.0, y + 8.0),
            Align2::LEFT_CENTER,
            "Open tabs",
            FontId::proportional(11.5),
            t.ink3,
        );
        if n > 0 {
            p.text(
                egui::pos2(left + w - 8.0, y + 8.0),
                Align2::RIGHT_CENTER,
                n.to_string(),
                FontId::proportional(11.5),
                t.ink4,
            );
        }
        y += 22.0;
        let foot = Rect::from_min_max(
            egui::pos2(left, r.bottom() - ROW_H - 10.0),
            egui::pos2(left + w, r.bottom() - 10.0),
        );
        let list = Rect::from_min_max(
            egui::pos2(r.left(), y),
            egui::pos2(r.right(), foot.top() - 10.0),
        );
        if n == 0 {
            p.text(
                egui::pos2(left + 8.0, y + 12.0),
                Align2::LEFT_CENTER,
                "None open",
                FontId::proportional(12.5),
                t.ink4,
            );
        } else {
            ui.scope_builder(UiBuilder::new().max_rect(list), |ui| self.side_tabs(ui, t));
        }
        p.hline(left..=left + w, foot.top() - 5.5, Stroke::new(1.0, t.line));
        let on = self.page == Page::Settings;
        if nav_row(
            ui,
            &p,
            t,
            foot,
            crate::composer::GEAR_ICON,
            "Settings",
            "",
            on,
        ) {
            if on {
                self.page = self.settings_from.take().unwrap_or(Page::Home);
            } else {
                self.settings_from = Some(self.page);
                self.page = Page::Settings;
            }
        }
    }

    /// The open tabs as rows, scrolling when they don't fit.
    fn side_tabs(&mut self, ui: &mut Ui, t: &Theme) {
        let current = match (&self.screen, self.page) {
            (Screen::Chat(id), Page::Chats) => Some(id.clone()),
            _ => None,
        };
        let font = FontId::proportional(13.0);
        let (mut show, mut close, mut rename, mut fork) = (None, None, None, None);
        let (can_reopen, mut reopen) = (!self.closed_tabs.is_empty(), false);
        let mut rows: Vec<Rect> = vec![];
        let pointer = ui.input(|i| {
            if i.pointer.primary_down() {
                i.pointer.interact_pos()
            } else {
                None
            }
        });
        let mut grabbed = None;
        let n = self.settings.tabs.len();
        let dragging = self.tab_drag.as_ref().map(|(id, _)| id.clone());
        let reveal_id = Id::unique("side-tab-reveal");
        let reveal = ui.data_mut(|d| {
            let last = d.get_temp_mut_or_default::<Option<String>>(reveal_id);
            let changed = *last != current;
            *last = current.clone();
            changed
        });
        ScrollArea::vertical().id_salt("side-tabs").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for i in 0..n {
                let id = self.settings.tabs[i].clone();
                let on = current.as_deref() == Some(id.as_str());
                let lifted = dragging.as_deref() == Some(id.as_str());
                ui.horizontal(|ui| {
                    ui.add_space(PAD);
                    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width() - PAD, ROW_H), Sense::hover());
                    rows.push(row);
                    if reveal && on {
                        ui.scroll_to_rect(row, None);
                    }
                    if !ui.is_rect_visible(row) {
                        return;
                    }
                    let (title, paused, busy, icon) = self.tab_look(&id);
                    let resp = ui.interact(row, Id::unique(("side-tab", &id)), Sense::click_and_drag());
                    let p = ui.painter();
                    let xr = Rect::from_center_size(egui::pos2(row.right() - 16.0, row.center().y), vec2(20.0, 20.0));
                    let x_resp = ui.interact(xr, Id::unique(("side-tab-x", &id)), Sense::click()).on_hover_text(format!("Close tab\n{title}"));
                    let hovered = resp.hovered() || x_resp.hovered();
                    if lifted {
                        p.rect_filled(row, 8.0, crate::gravity::selected());
                        p.rect_stroke(row, 8.0, Stroke::new(1.0, t.accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                    } else if on {
                        p.rect_filled(row, 8.0, crate::gravity::selected());
                        // The selected tab's accent edge.
                        let bar = Rect::from_min_size(egui::pos2(row.left() + 1.0, row.center().y - 7.0), vec2(3.0, 14.0));
                        p.rect_filled(bar, 1.5, t.accent);
                    } else if hovered {
                        p.rect_filled(row, 8.0, mix(crate::gravity::shell(), t.ink, 0.05));
                    }
                    let mut tx = row.left() + 12.0;
                    if busy || paused {
                        state_dot(p, t, egui::pos2(tx + 3.0, row.center().y), paused);
                        tx += 13.0;
                    }
                    if let Some((icon, font)) = icon {
                        p.text(egui::pos2(tx, row.center().y), Align2::LEFT_CENTER, icon, font, if on { t.accent } else { t.ink3 });
                        tx += 19.0;
                    }
                    let room = if hovered || on { xr.left() - 4.0 } else { row.right() - 8.0 } - tx;
                    let color = if on || hovered { t.ink } else { t.ink2 };
                    p.text(egui::pos2(tx, row.center().y), Align2::LEFT_CENTER, clip_width(ui, &title, &font, room), font.clone(), color);
                    if hovered || on {
                        if x_resp.hovered() {
                            p.rect_filled(xr, 6.0, t.panel2);
                        }
                        draw_x(p, xr, if x_resp.hovered() { t.ink } else { t.ink3 });
                    }
                    let resp = resp.on_hover_text(&title);
                    let renamable = self.can_rename(&id);
                    resp.context_menu(|ui| {
                        if renamable {
                            if ui.button("Rename\u{2026}").clicked() {
                                rename = Some(id.clone());
                                ui.close();
                            }
                            ui.separator();
                        }
                        if self.sessions.contains_key(&id) && ui.button("Fork conversation").on_hover_text("A new conversation with this one's transcript and notes, and no running work. Files are shared.").clicked() {
                            fork = Some(id.clone());
                            ui.close();
                        }
                        let any_inactive = self.settings.tabs.iter().any(|id| self.tab_inactive(id));
                        if let Some(action) = crate::tabs::tab_menu(ui, i, n, on, any_inactive, can_reopen, &mut reopen) {
                            close = Some((i, action));
                        }
                    });
                    if resp.drag_started() {
                        grabbed = Some((id.clone(), 0.0));
                    }
                    if x_resp.clicked() || resp.middle_clicked() || x_resp.middle_clicked() {
                        close = Some((i, TabClose::One));
                    } else if resp.clicked() {
                        show = Some(id.clone());
                    }
                });
            }
        });
        if grabbed.is_some() {
            self.tab_drag = grabbed;
        }
        // A tab being dragged takes the place of the row the pointer is
        // over; letting go leaves it there.
        match (self.tab_drag.clone(), pointer) {
            (Some((id, _)), Some(pointer)) if self.settings.tabs.contains(&id) => {
                let from = self.settings.tabs.iter().position(|t| *t == id).unwrap();
                if ui.max_rect().contains(pointer)
                    && let Some(to) = rows.iter().position(|r| r.y_range().contains(pointer.y))
                    && to != from
                {
                    let id = self.settings.tabs.remove(from);
                    self.settings.tabs.insert(to, id);
                }
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                return;
            }
            (Some(_), _) => {
                self.tab_drag = None;
                store::save_settings(&self.settings);
            }
            (None, _) => {}
        }
        if let Some(id) = rename {
            self.start_rename(&id);
        }
        if let Some(id) = fork {
            self.fork_conversation(&id, None, ui.ctx());
        }
        if let Some(id) = show {
            self.open(id);
        }
        if let Some((i, action)) = close {
            self.close_tabs(i, action);
        }
        if reopen {
            self.reopen_tab();
        }
    }
}

/// The New conversation pill: lit while the New screen is shown (`on`).
/// Whether it was clicked.
fn pill_button(ui: &Ui, p: &Painter, t: &Theme, rect: Rect, on: bool) -> bool {
    let resp = ui
        .interact(rect, Id::unique("side-new"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let shell = crate::gravity::shell();
    let (fill, line) = match (on, resp.hovered()) {
        (true, _) => (mix(shell, t.accent, 0.16), t.accent.gamma_multiply(0.7)),
        (false, true) => (mix(shell, t.ink, 0.10), t.ink4),
        _ => (mix(shell, t.ink, 0.06), t.line),
    };
    p.rect_filled(rect, 10.0, fill);
    p.rect_stroke(rect, 10.0, Stroke::new(1.0, line), egui::StrokeKind::Inside);
    p.text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        Align2::LEFT_CENTER,
        PLUS_ICON,
        markdown::icon_font(12.0),
        t.ink,
    );
    p.text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        Align2::LEFT_CENTER,
        "New conversation",
        FontId::proportional(13.5),
        t.ink,
    );
    if rect.width() >= 200.0 {
        p.text(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            Align2::RIGHT_CENTER,
            "Ctrl+T",
            FontId::proportional(11.0),
            t.ink4,
        );
    }
    resp.on_hover_text("New conversation (Ctrl+T)").clicked()
}

/// A nav row: its icon, label and (on hover) key hint; lit when `on`.
/// Whether it was clicked.
#[allow(clippy::too_many_arguments)]
fn nav_row(
    ui: &Ui,
    p: &Painter,
    t: &Theme,
    rect: Rect,
    icon: &str,
    label: &str,
    hint: &str,
    on: bool,
) -> bool {
    let resp = ui
        .interact(rect, Id::unique(("side-nav", label)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if on {
        p.rect_filled(rect, 8.0, crate::gravity::selected());
    } else if resp.hovered() {
        p.rect_filled(rect, 8.0, mix(crate::gravity::shell(), t.ink, 0.05));
    }
    let ink = if on || resp.hovered() { t.ink } else { t.ink2 };
    p.text(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        markdown::icon_font(13.0),
        if on { t.accent } else { t.ink3 },
    );
    p.text(
        egui::pos2(rect.left() + 36.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        ink,
    );
    if !hint.is_empty() && rect.width() >= 185.0 && resp.hovered() {
        p.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            hint,
            FontId::proportional(11.0),
            t.ink4,
        );
    }
    resp.clicked()
}
