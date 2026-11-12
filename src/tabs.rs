//! The tab strip under the title bar: Home's forest tab, a tab for each open
//! conversation (drag to move, x or a middle click to close, right-click to
//! rename it or close tabs), + for a new one, scrolling when they don't fit, and the
//! keyboard shortcuts browsers have for tabs.
use super::*;
use crate::widgets::{clip_width, draw_x, fade, state_dot};
use egui::Painter;

/// The tab strip's height.
pub(crate) const TABS_H: f32 = 34.0;
/// A tab's width; the forest tab, which is Home, is as wide.
pub(crate) const TAB_W: f32 = 210.0;
/// With many tabs they get narrower, down to this; past that the strip
/// scrolls from side to side.
const TAB_MIN_W: f32 = 120.0;
/// The + at the end of the strip.
pub(crate) const PLUS_W: f32 = 36.0;

/// How many closed tabs Ctrl+Shift+T can bring back.
const CLOSED_KEPT: usize = 50;

/// Which tabs a close command affects, relative to the tab right-clicked.
#[derive(Clone, Copy)]
pub(crate) enum TabClose {
    One,
    Others,
    Left,
    Right,
    All,
}

impl TabClose {
    pub(crate) fn includes(self, index: usize, anchor: usize) -> bool {
        match self {
            Self::One => index == anchor,
            Self::Others => index != anchor,
            Self::Left => index < anchor,
            Self::Right => index > anchor,
            Self::All => true,
        }
    }
}
/// The tab shortcuts, as browsers have them. Home counts as the first tab.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum TabKey {
    /// Ctrl+W or Ctrl+F4: close the tab shown.
    Close,
    /// Ctrl+Tab or Ctrl+PageDown, and back with Ctrl+Shift+Tab or Ctrl+PageUp.
    Next,
    Prev,
    /// Ctrl+1 to Ctrl+8: that tab, Home being 0.
    Select(usize),
    /// Ctrl+9: the last tab.
    Last,
    /// Ctrl+Shift+T: the tab closed last, back where it was.
    Reopen,
}

/// The tab shortcut `key` with `m` held makes, if any. Exactly Ctrl (and
/// Shift where it says): Ctrl+Alt is AltGr typing a character on Windows.
pub(crate) fn tab_key(key: Key, m: Modifiers) -> Option<TabKey> {
    if !m.command || m.alt {
        return None;
    }
    Some(match (key, m.shift) {
        (Key::W | Key::F4, false) => TabKey::Close,
        (Key::Tab, false) | (Key::PageDown, false) => TabKey::Next,
        (Key::Tab, true) | (Key::PageUp, false) => TabKey::Prev,
        (Key::T, true) => TabKey::Reopen,
        (Key::Num9, false) => TabKey::Last,
        (k, false) => TabKey::Select([Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8].iter().position(|&d| d == k)?),
        _ => return None,
    })
}

/// Where Ctrl+Tab (`forward`) or Ctrl+Shift+Tab goes from tab `at` (0 is
/// Home, `n + 1` the New screen past the `n` tabs), round the ends.
pub(crate) fn cycle(at: usize, n: usize, forward: bool) -> usize {
    match (forward, at) {
        (true, a) if a >= n => 0,
        (true, a) => a + 1,
        (false, 0) => n,
        (false, a) => (a - 1).min(n),
    }
}

impl App {
    /// Whether `id` can have a tab: its conversation or cloud session is
    /// still there, or the plugin whose tab it is says so.
    fn tab_exists(&self, id: &str) -> bool {
        self.sessions.get(id).is_some_and(|s| s.conv.lock().unwrap().side_chat_of.is_none()) || cloud::is_key(id) || crate::plugins::tab_kind(id).is_some_and(|k| k.exists(id))
    }

    /// Takes this frame's tab shortcuts out of the input (so the text boxes
    /// and the browser pane never see them) and does them. Ctrl+W closes the
    /// tab even in a text box, as in a browser: only the tab, which
    /// Ctrl+Shift+T brings back (egui's Ctrl+W deleting a word goes;
    /// Ctrl+Backspace does that).
    fn tab_keys(&mut self, ui: &Ui) {
        let keys: Vec<TabKey> = ui.input_mut(|i| {
            let mut keys = vec![];
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => match tab_key(*key, *modifiers) {
                    Some(k) => {
                        keys.push(k);
                        false
                    }
                    None => true,
                },
                _ => true,
            });
            keys
        });
        for k in keys {
            let n = self.settings.tabs.len();
            // Where we are: Home (and the pages under it) is 0, a tab its
            // place after Home, the New screen past the last tab.
            let at = match (&self.screen, self.page) {
                (Screen::Chat(id), Page::Chats) => self.settings.tabs.iter().position(|t| t == id).map_or(0, |i| i + 1),
                (Screen::New, Page::Chats) => n + 1,
                _ => 0,
            };
            let to = match k {
                TabKey::Close if self.side_chat_focused() => {
                    self.close_side_chat();
                    continue;
                }
                TabKey::Close => {
                    if (1..=n).contains(&at) {
                        self.close_tabs(at - 1, TabClose::One);
                    }
                    continue;
                }
                TabKey::Reopen => {
                    self.reopen_tab();
                    continue;
                }
                TabKey::Next => cycle(at, n, true),
                TabKey::Prev => cycle(at, n, false),
                TabKey::Select(i) if i <= n => i,
                TabKey::Select(_) => continue,
                TabKey::Last => n,
            };
            match to {
                0 => self.page = Page::Home,
                i => self.open(self.settings.tabs[i - 1].clone()),
            }
        }
    }

    /// Reopens the tab closed last that still has something to show, where
    /// it was, and shows it.
    pub(crate) fn reopen_tab(&mut self) {
        while let Some((at, id)) = self.closed_tabs.pop() {
            if !self.tab_exists(&id) {
                continue;
            }
            if !self.settings.tabs.contains(&id) {
                self.settings.tabs.insert(at.min(self.settings.tabs.len()), id.clone());
            }
            self.open(id);
            return;
        }
    }

    /// Closes only the tabs, not their sessions, drafts or running work;
    /// Ctrl+Shift+T brings them back.
    pub(crate) fn close_tabs(&mut self, anchor: usize, action: TabClose) {
        if anchor >= self.settings.tabs.len() {
            return;
        }
        // As if closed from right to left, so each place counts only the
        // tabs left of it, and reopening (newest first) puts each back.
        for (i, id) in self.settings.tabs.iter().enumerate().rev() {
            if action.includes(i, anchor) {
                self.closed_tabs.push((i, id.clone()));
            }
        }
        let extra = self.closed_tabs.len().saturating_sub(CLOSED_KEPT);
        self.closed_tabs.drain(..extra);
        let selected = match &self.screen {
            Screen::Chat(id) => self.settings.tabs.iter().position(|tab| tab == id),
            Screen::New => None,
        };
        // If the selected tab closes, prefer its nearest surviving tab on
        // the right, then the left. Count its new position before removal.
        let next = selected.filter(|&i| action.includes(i, anchor)).map(|i| {
            (0..i).filter(|&j| !action.includes(j, anchor)).count()
        });
        let mut index = 0;
        self.settings.tabs.retain(|_| {
            index += 1;
            !action.includes(index - 1, anchor)
        });
        store::save_settings(&self.settings);
        self.tab_drag = None;
        if let Some(next) = next {
            match self.settings.tabs.get(next.min(self.settings.tabs.len().saturating_sub(1))) {
                Some(id) => self.screen = Screen::Chat(id.clone()),
                None => {
                    self.screen = Screen::New;
                    if self.page == Page::Chats {
                        self.page = Page::Home;
                    }
                }
            }
        }
    }

    /// What `id`'s tab shows: its title, whether it is paused and whether
    /// at work, and the icon before its title (with its font): a cloud
    /// session's provider, a plugin tab's, or its role's.
    fn tab_look(&self, id: &str) -> (String, bool, bool, Option<(String, FontId)>) {
        if let Some(s) = self.sessions.get(id) {
            let c = s.conv.lock().unwrap();
            let title = if c.title.is_empty() { "Untitled".to_string() } else { c.title.clone() };
            // (A plugin's role may mark its conversations' tabs.)
            let icon = Some(crate::agent::prompt::role(&c.role).icon).filter(|i| !i.is_empty()).map(|i| (i.to_string(), markdown::icon_font(13.0)));
            return (title, c.paused, s.live.lock().unwrap().busy, icon);
        }
        if let Some(kind) = crate::plugins::tab_kind(id) {
            let look = kind.look(id);
            let font = if look.icon_is_text { FontId::proportional(14.0) } else { markdown::icon_font(13.0) };
            return (look.title, false, look.busy, Some((look.icon, font)).filter(|(i, _)| !i.is_empty()));
        }
        let (title, busy) = cloud::tab_info(id);
        (title, false, busy, cloud::is_key(id).then(|| provider_icon(cloud::provider(id), 12.0)))
    }

    /// The tab strip: a tab per open conversation (click to show, drag to
    /// move, x or middle-click to close, right-click for more), and + for a
    /// new one. Alt+H goes Home; Ctrl+T opens a new conversation; the
    /// browsers' tab shortcuts (`TabKey`) close, cycle, pick and reopen. When the
    /// tabs don't fit, the wheel or the touchpad over them scrolls them
    /// from side to side.
    pub(crate) fn tab_bar(&mut self, ui: &mut Ui, t: &Theme) {
        self.tab_bar_keys(ui);
        let r = ui.max_rect();
        let p = ui.painter().clone();
        p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, t.line));
        let current = match (&self.screen, self.page) {
            (Screen::Chat(id), Page::Chats) => Some(id.clone()),
            _ => None,
        };
        let forest = self.forest_tab(ui, &p, t, r);
        let s = Strip::new(r, forest, self.settings.tabs.len());
        let pointer = ui.input(|i| if i.pointer.primary_down() { i.pointer.interact_pos() } else { None });
        self.scroll_tabs(ui, &s, pointer, &current);
        let dragged = self.drag_tab(ui, &s, pointer);
        // The tabs are drawn clipped to their room; + and the rest aren't.
        let (show, close, reopen) = self.tabs(ui, &p.with_clip_rect(s.area.intersect(p.clip_rect())), t, &s, dragged, &current);
        self.plus_tab(ui, &p, t, &s);
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

    /// Alt+H, the browsers' tab shortcuts and Ctrl+T; and the tabs whose
    /// conversation, session, bot or agent has gone are dropped.
    fn tab_bar_keys(&mut self, ui: &mut Ui) {
        if ui.input_mut(|i| i.consume_key(Modifiers::ALT, Key::H)) {
            self.page = Page::Home;
        }
        let tabs = std::mem::take(&mut self.settings.tabs);
        self.settings.tabs = tabs.into_iter().filter(|id| self.tab_exists(id)).collect();
        // Before Ctrl+T, which Ctrl+Shift+T would match too.
        self.tab_keys(ui);
        // Ctrl+T: a new conversation, as + does, with the message box focused.
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T)) {
            self.screen = Screen::New;
            self.page = Page::Chats;
            ui.memory_mut(|m| m.request_focus(Id::unique("composer")));
        }
    }

    /// The forest: Home, as the first tab at the left of the strip `r`.
    fn forest_tab(&mut self, ui: &Ui, p: &Painter, t: &Theme, r: Rect) -> Rect {
        let forest = Rect::from_min_size(r.min, vec2(TAB_W.min(r.width() / 3.0), r.height() - 1.0));
        let resp = ui.interact(forest, Id::unique("tab-forest"), Sense::click());
        let home = self.page == Page::Home;
        let tex = picture(ui.ctx(), &mut self.pictures, "forest-header");
        paint_cover(p, forest, &tex, if home || resp.hovered() { Color32::WHITE } else { Color32::from_gray(190) });
        if home {
            p.hline(forest.x_range(), forest.bottom() - 1.0, Stroke::new(2.0, t.accent));
        }
        p.vline(forest.right(), forest.y_range(), Stroke::new(1.0, t.line));
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Home (Alt+H)").clicked() {
            self.page = Page::Home;
        }
        forest
    }

    /// Scrolls the tabs: by the wheel or the touchpad over them, near an end
    /// while one is dragged there, and to bring the tab shown into view.
    fn scroll_tabs(&mut self, ui: &Ui, s: &Strip, pointer: Option<egui::Pos2>, current: &Option<String>) {
        // The wheel, or the touchpad either way, over the tabs.
        if s.max_scroll > 0.0 && ui.rect_contains_pointer(s.area) {
            let d = ui.input_mut(|i| std::mem::take(&mut i.smooth_scroll_delta));
            self.tab_scroll -= d.x + d.y;
        }
        // Dragged near an end, the tabs scroll under the dragged one: only
        // towards the end it is being dragged to (so grabbing an end tab to
        // drag it the other way doesn't scroll).
        let dragging = self.tab_drag.as_ref().is_some_and(|(id, _)| self.settings.tabs.contains(id)) && pointer.is_some();
        if dragging && s.max_scroll > 0.0 && let Some(pointer) = pointer {
            const EDGE: f32 = 40.0;
            let moved = ui.input(|i| i.pointer.press_origin()).map_or(0.0, |o| pointer.x - o.x);
            let right = if moved > 0.0 { (pointer.x - (s.area.right() - EDGE)).max(0.0) } else { 0.0 };
            let left = if moved < 0.0 { (s.area.left() + EDGE - pointer.x).max(0.0) } else { 0.0 };
            let push = right - left;
            if push != 0.0 {
                let dt = ui.input(|i| i.stable_dt).min(0.1);
                self.tab_scroll += push.clamp(-EDGE, EDGE) * 20.0 * dt;
                ui.ctx().request_repaint();
            }
        }
        // The tab shown is scrolled into view, clear of the faded ends, when
        // it becomes the one shown and when the strip changes size.
        if !dragging && (*current != self.tab_revealed || self.tab_revealed_at != (s.room, s.w)) {
            if let Some(i) = current.as_ref().and_then(|c| self.settings.tabs.iter().position(|t| t == c)) {
                let left = i as f32 * s.w;
                self.tab_scroll = self.tab_scroll.max(left + s.w - s.room + FADE).min(left - FADE);
            }
            self.tab_revealed = current.clone();
            self.tab_revealed_at = (s.room, s.w);
        }
        self.tab_scroll = self.tab_scroll.clamp(0.0, s.max_scroll);
    }

    /// A tab being dragged follows the pointer and takes the place its
    /// middle is over; the others make room. Letting go leaves it there.
    /// Gives the dragged tab's place and where it is drawn.
    fn drag_tab(&mut self, ui: &Ui, s: &Strip, pointer: Option<egui::Pos2>) -> Option<(usize, Rect)> {
        match (self.tab_drag.clone(), pointer) {
            (Some((id, grab)), Some(pointer)) if self.settings.tabs.contains(&id) => {
                let (n, w, first) = (s.n, s.w, s.slot(0, self.tab_scroll).left());
                let from = self.settings.tabs.iter().position(|t| *t == id).unwrap();
                // Within the tabs: never past + when they don't fill the strip.
                let end = s.area.right().min(first + n as f32 * w);
                let left = (pointer.x - grab).clamp(s.area.left(), (end - w).max(s.area.left()));
                let to = (((left + w / 2.0 - first) / w).floor().max(0.0) as usize).min(n - 1);
                if to != from {
                    let id = self.settings.tabs.remove(from);
                    self.settings.tabs.insert(to, id);
                }
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                Some((to, Rect::from_min_size(egui::pos2(left, s.r.top()), vec2(w, s.r.height() - 1.0))))
            }
            (Some(_), _) => {
                self.tab_drag = None;
                store::save_settings(&self.settings);
                None
            }
            (None, _) => None,
        }
    }

    /// The tabs in view, on `p` (clipped to their room), with the faded ends
    /// where there are more to scroll to. Gives the tab clicked to show, the
    /// close asked for and whether to reopen a closed tab.
    #[allow(clippy::type_complexity)]
    fn tabs(&mut self, ui: &Ui, p: &Painter, t: &Theme, s: &Strip, dragged: Option<(usize, Rect)>, current: &Option<String>) -> (Option<String>, Option<(usize, TabClose)>, bool) {
        let (area, n) = (s.area, s.n);
        let font = FontId::proportional(12.0);
        let (mut show, mut close) = (None, None);
        // The tabs in view; the dragged one is drawn last, over the others.
        let order: Vec<usize> = (0..n)
            .filter(|&i| Some(i) != dragged.map(|d| d.0) && s.slot(i, self.tab_scroll).intersects(area))
            .chain(dragged.map(|d| d.0))
            .collect();
        let mut grabbed = None;
        let (can_reopen, mut reopen) = (!self.closed_tabs.is_empty(), false);
        let mut rename = None;
        for i in order {
            let id = &self.settings.tabs[i];
            let lifted = dragged.is_some_and(|d| d.0 == i);
            let (title, paused, busy, icon) = self.tab_look(id);
            let on = current.as_deref() == Some(id.as_str());
            let tab = dragged.filter(|d| d.0 == i).map_or(s.slot(i, self.tab_scroll), |d| d.1);
            // A tab half scrolled out answers only where it shows.
            let resp = ui.interact(tab.intersect(area), Id::unique(("tab", id)), Sense::click_and_drag()).on_hover_text(&title);
            let renamable = self.can_rename(id);
            resp.context_menu(|ui| {
                if renamable {
                    if ui.button("Rename…").clicked() {
                        rename = Some(id.clone());
                        ui.close();
                    }
                    ui.separator();
                }
                if let Some(action) = tab_menu(ui, i, n, on, can_reopen, &mut reopen) {
                    close = Some((i, action));
                }
            });
            let xr = Rect::from_center_size(egui::pos2(tab.right() - 15.0, tab.center().y), vec2(20.0, 20.0));
            let x_resp = ui.interact(xr.intersect(area), Id::unique(("tab-x", id)), Sense::click()).on_hover_text(format!("Close tab\n{title}"));
            if resp.drag_started()
                && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            {
                grabbed = Some((id.clone(), origin.x - tab.left()));
            }
            if lifted {
                // Solid, so the tabs under it don't show through.
                p.rect_filled(tab, 0.0, t.bg);
                p.vline(tab.left(), tab.y_range(), Stroke::new(1.0, t.line));
            } else if on {
                p.rect_filled(tab, 0.0, t.panel);
                p.hline(tab.x_range(), tab.bottom() - 1.0, Stroke::new(2.0, t.accent));
            } else if resp.hovered() || x_resp.hovered() {
                p.rect_filled(tab, 0.0, t.tool_hover);
            }
            p.vline(tab.right(), tab.y_range(), Stroke::new(1.0, t.line));
            let mut tx = tab.left() + 12.0;
            if busy || paused {
                state_dot(p, t, egui::pos2(tx + 3.0, tab.center().y), paused);
                tx += 12.0;
            }
            if let Some((icon, font)) = icon {
                p.text(egui::pos2(tx, tab.center().y), Align2::LEFT_CENTER, icon, font, if on { t.accent } else { t.ink3 });
                tx += 18.0;
            }
            let color = if on || resp.hovered() { t.ink } else { t.ink3 };
            p.text(egui::pos2(tx, tab.center().y), Align2::LEFT_CENTER, clip_width(ui, &title, &font, xr.left() - tx - 6.0), font.clone(), color);
            if on || resp.hovered() || x_resp.hovered() {
                if x_resp.hovered() {
                    p.rect_filled(xr, 0.0, t.panel2);
                }
                draw_x(p, xr, if x_resp.hovered() { t.ink } else { t.ink3 });
            }
            // A middle click anywhere on the tab, its x's spot included.
            if x_resp.clicked() || resp.middle_clicked() || x_resp.middle_clicked() {
                close = Some((i, TabClose::One));
            } else if resp.clicked() {
                show = Some(id.clone());
            }
        }
        if grabbed.is_some() {
            self.tab_drag = grabbed;
        }
        // Faded ends where there are more tabs to scroll to.
        let ends = s.r.top()..=s.r.bottom() - 1.0;
        if self.tab_scroll > 0.5 {
            fade(p, Rect::from_x_y_ranges(area.left()..=area.left() + FADE, ends.clone()), t.bg, true);
        }
        if self.tab_scroll < s.max_scroll - 0.5 {
            fade(p, Rect::from_x_y_ranges(area.right() - FADE..=area.right(), ends), t.bg, false);
        }
        if dragged.is_some() {
            // No closing or switching mid-drag.
            return (None, None, false);
        }
        if let Some(id) = rename {
            self.start_rename(&id);
        }
        (show, close, reopen)
    }

    /// The + tab: a new conversation, after the tabs (at the right end when
    /// they scroll).
    fn plus_tab(&mut self, ui: &Ui, p: &Painter, t: &Theme, s: &Strip) {
        let (r, area) = (s.r, s.area);
        let plus = Rect::from_min_size(egui::pos2((area.left() + s.n as f32 * s.w).min(area.right()), r.top()), vec2(PLUS_W, r.height() - 1.0));
        let resp = ui.interact(plus, Id::unique("tab-new"), Sense::click()).on_hover_text("New conversation (Ctrl+T)");
        let on = self.page == Page::Chats && matches!(self.screen, Screen::New);
        if on || resp.hovered() {
            p.rect_filled(plus, 0.0, if on { t.panel } else { t.tool_hover });
        }
        if on {
            p.hline(plus.x_range(), plus.bottom() - 1.0, Stroke::new(2.0, t.accent));
        }
        p.text(plus.center(), Align2::CENTER_CENTER, "+", FontId::proportional(15.0), if on || resp.hovered() { t.ink } else { t.ink3 });
        if s.max_scroll > 0.0 {
            p.vline(plus.left(), plus.y_range(), Stroke::new(1.0, t.line));
        }
        if resp.clicked() {
            self.screen = Screen::New;
            self.page = Page::Chats;
        }
    }
}

/// How far in from each end of the strip the tabs fade when there are more
/// to scroll to.
const FADE: f32 = 28.0;

/// Where the tabs go in the strip `r`: the `area` between Home and +, `room`
/// wide, that the `n` tabs share, `w` each (TAB_W, narrower down to
/// TAB_MIN_W when there are many; past that they scroll, by up to
/// `max_scroll`, clipped to that room, with + kept at the right end).
struct Strip {
    r: Rect,
    area: Rect,
    room: f32,
    w: f32,
    n: usize,
    max_scroll: f32,
}

impl Strip {
    fn new(r: Rect, forest: Rect, n: usize) -> Self {
        let room = (r.right() - PLUS_W - forest.right()).max(TAB_MIN_W);
        let w = if n == 0 { TAB_W } else { (room / n as f32).clamp(TAB_MIN_W, TAB_W).floor() };
        let area = Rect::from_x_y_ranges(forest.right()..=forest.right() + room, r.top()..=r.bottom() - 1.0);
        Self { r, area, room, w, n, max_scroll: (n as f32 * w - room).max(0.0) }
    }

    /// Tab `i`'s place with the tabs scrolled by `scroll`.
    fn slot(&self, i: usize, scroll: f32) -> Rect {
        Rect::from_min_size(egui::pos2(self.area.left() + i as f32 * self.w - scroll, self.r.top()), vec2(self.w, self.r.height() - 1.0))
    }
}

/// A tab's right-click menu: the closes (`n` tabs, this one `i`, `on` if
/// shown) and reopening a closed tab, which sets `reopen`. Gives the close
/// picked.
fn tab_menu(ui: &mut Ui, i: usize, n: usize, on: bool, can_reopen: bool, reopen: &mut bool) -> Option<TabClose> {
    let mut picked = None;
    for (label, action, enabled, divider) in [
        ("Close tab", TabClose::One, true, false),
        ("Close others", TabClose::Others, n > 1, false),
        ("Close tabs to the left", TabClose::Left, i > 0, false),
        ("Close tabs to the right", TabClose::Right, i + 1 < n, false),
        ("Close all tabs", TabClose::All, true, true),
    ] {
        if divider {
            ui.separator();
        }
        // Ctrl+W closes the tab shown.
        let keys = if on && matches!(action, TabClose::One) { "Ctrl+W" } else { "" };
        if ui.add_enabled(enabled, Button::new(label).shortcut_text(keys)).clicked() {
            picked = Some(action);
            ui.close();
        }
    }
    ui.separator();
    if ui.add_enabled(can_reopen, Button::new("Reopen closed tab").shortcut_text("Ctrl+Shift+T")).clicked() {
        *reopen = true;
        ui.close();
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{Headless, conv_json, key};

    /// The browsers' tab shortcuts, with exactly Ctrl (and Shift where they
    /// take it): Ctrl+Alt, which is AltGr, and other keys are left alone.
    #[test]
    fn tab_shortcuts() {
        let (ctrl, shift) = (Modifiers::COMMAND, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(tab_key(Key::W, ctrl), Some(TabKey::Close));
        assert_eq!(tab_key(Key::F4, ctrl), Some(TabKey::Close));
        assert_eq!(tab_key(Key::Tab, ctrl), Some(TabKey::Next));
        assert_eq!(tab_key(Key::PageDown, ctrl), Some(TabKey::Next));
        assert_eq!(tab_key(Key::Tab, shift), Some(TabKey::Prev));
        assert_eq!(tab_key(Key::PageUp, ctrl), Some(TabKey::Prev));
        assert_eq!(tab_key(Key::T, shift), Some(TabKey::Reopen));
        assert_eq!(tab_key(Key::Num1, ctrl), Some(TabKey::Select(0)));
        assert_eq!(tab_key(Key::Num8, ctrl), Some(TabKey::Select(7)));
        assert_eq!(tab_key(Key::Num9, ctrl), Some(TabKey::Last));
        for (key, m) in [
            (Key::T, ctrl),
            (Key::Num0, ctrl),
            (Key::W, shift),
            (Key::PageDown, shift),
            (Key::Num7, Modifiers::COMMAND | Modifiers::ALT),
            (Key::W, Modifiers::NONE),
            (Key::Tab, Modifiers::NONE),
            (Key::Tab, Modifiers::SHIFT),
            (Key::PageUp, Modifiers::NONE),
        ] {
            assert_eq!(tab_key(key, m), None, "{key:?} {m:?}");
        }
        // Home is 0, the tabs 1 to 3, the New screen 4: round the ends.
        assert_eq!([0, 1, 3, 4].map(|a| cycle(a, 3, true)), [1, 2, 0, 0]);
        assert_eq!([0, 1, 3, 4].map(|a| cycle(a, 3, false)), [3, 0, 2, 3]);
        assert_eq!((cycle(0, 0, true), cycle(0, 0, false)), (0, 0));
    }

    /// In the whole app: Ctrl+Tab and Ctrl+PageUp go round Home and the
    /// tabs, Ctrl+N picks one, Ctrl+W closes the tab shown (even typing in
    /// the message box, whose text stays), and Ctrl+Shift+T brings closed
    /// tabs back in their places, newest first.
    #[test]
    #[ignore]
    fn whole_app_tab_shortcuts() {
        let ids = ["tabkeys-a", "tabkeys-b", "tabkeys-c"];
        let (dir, mut h, mut app) = Headless::start("tab-keys", &ids.map(|id| conv_json(id, id, serde_json::json!([]))));
        app.settings.tabs = ids.iter().map(|s| s.to_string()).collect();
        let press = |h: &mut Headless, app: &mut App, k, modifiers| {
            h.events(app, vec![key(k, modifiers)]);
        };
        let shown = |app: &App| match (&app.screen, app.page) {
            (Screen::Chat(id), Page::Chats) => id.clone(),
            (Screen::New, Page::Chats) => "new".into(),
            _ => "home".into(),
        };
        let (ctrl, shift) = (Modifiers::COMMAND, Modifiers::COMMAND | Modifiers::SHIFT);
        press(&mut h, &mut app, Key::Num1, Modifiers::NONE);
        assert_eq!(shown(&app), "home");
        press(&mut h, &mut app, Key::Tab, ctrl);
        assert_eq!(shown(&app), "tabkeys-a");
        press(&mut h, &mut app, Key::PageDown, ctrl);
        assert_eq!(shown(&app), "tabkeys-b");
        press(&mut h, &mut app, Key::Tab, shift);
        assert_eq!(shown(&app), "tabkeys-a");
        press(&mut h, &mut app, Key::PageUp, ctrl);
        assert_eq!(shown(&app), "home");
        press(&mut h, &mut app, Key::PageUp, ctrl);
        assert_eq!(shown(&app), "tabkeys-c");
        press(&mut h, &mut app, Key::Num2, ctrl);
        assert_eq!(shown(&app), "tabkeys-a");
        press(&mut h, &mut app, Key::Num9, ctrl);
        assert_eq!(shown(&app), "tabkeys-c");
        press(&mut h, &mut app, Key::Num1, ctrl);
        assert_eq!(shown(&app), "home");
        press(&mut h, &mut app, Key::Num7, ctrl);
        assert_eq!(shown(&app), "home");
        // Ctrl+W on Home closes nothing.
        press(&mut h, &mut app, Key::W, ctrl);
        assert_eq!(app.settings.tabs.len(), 3);
        // Typing in b's message box, Ctrl+W closes b, not a word.
        press(&mut h, &mut app, Key::Num3, ctrl);
        assert_eq!(shown(&app), "tabkeys-b");
        h.ctx.memory_mut(|m| m.request_focus(Id::unique("composer")));
        h.events(&mut app, vec![egui::Event::Text("two words".into())]);
        press(&mut h, &mut app, Key::W, ctrl);
        assert_eq!(app.settings.tabs, ["tabkeys-a", "tabkeys-c"]);
        assert_eq!(shown(&app), "tabkeys-c");
        // Close a too, then bring both back where they were, newest first.
        press(&mut h, &mut app, Key::Num2, ctrl);
        press(&mut h, &mut app, Key::F4, ctrl);
        assert_eq!(app.settings.tabs, ["tabkeys-c"]);
        press(&mut h, &mut app, Key::T, shift);
        assert_eq!(app.settings.tabs, ["tabkeys-a", "tabkeys-c"]);
        assert_eq!(shown(&app), "tabkeys-a");
        press(&mut h, &mut app, Key::T, shift);
        assert_eq!(app.settings.tabs, ids);
        assert_eq!(shown(&app), "tabkeys-b");
        // Its draft survived the close.
        press(&mut h, &mut app, Key::Num2, ctrl);
        press(&mut h, &mut app, Key::Num3, ctrl);
        assert_eq!(app.input, "two words");
        // Nothing left to reopen: Ctrl+Shift+T isn't Ctrl+T.
        press(&mut h, &mut app, Key::T, shift);
        assert_eq!(shown(&app), "tabkeys-b");
        press(&mut h, &mut app, Key::T, ctrl);
        assert_eq!(shown(&app), "new");
        // From the New screen, round to Home and back to the last tab.
        press(&mut h, &mut app, Key::Tab, ctrl);
        assert_eq!(shown(&app), "home");
        press(&mut h, &mut app, Key::T, ctrl);
        press(&mut h, &mut app, Key::Tab, shift);
        assert_eq!(shown(&app), "tabkeys-c");
        // Close others, then reopen them in order.
        app.close_tabs(2, TabClose::Others);
        press(&mut h, &mut app, Key::T, shift);
        press(&mut h, &mut app, Key::T, shift);
        assert_eq!(app.settings.tabs, ids);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
