//! Antigravity's Home (`gravity.rs`): a desktop, Home and the conversations
//! on one page. Over the dimmed wallpaper, tiles in a grid: the
//! conversations (the search field and sort, chips for all of them, those at
//! work and those paused, and the list: what is running pinned at its top,
//! the rest under Today, Yesterday, This week and Earlier; a row lights
//! under the pointer and shows its quick actions, Rename, Fork and Delete),
//! the clock (the time, the date and an analog face whose ring fills with
//! the seconds in the logo's colors), this month's calendar (a dot under
//! each day a conversation was last active; a click on one lists that
//! day's), an overview (conversations, at work, paused, tabs open; a click
//! on a count filters the list) and the usage preview. At the bottom, a
//! dock in place of the corner buttons: the conversations' search, Home's
//! menu on its other lists (Cloud, Protocols, Plugins), New conversation
//! and Settings.
//!
//! The grid reflows from two columns to a stack as the pane narrows,
//! scrolling when it is too short to show every tile. Home's menu opens
//! over the desktop above the dock.

use super::*;
use crate::gravity::{self, MARKS};
use crate::home::ChatRow;
use crate::theme::mix;
use chrono::{Datelike, Local, NaiveDate, Timelike};
use oak_sdk::theme::hex;

/// Between tiles, and between them and the pane's edges.
const GAP: f32 = 16.0;
/// The dock's height, and the room it takes at the bottom of the pane.
const DOCK_H: f32 = 58.0;
const DOCK_ROOM: f32 = DOCK_H + 28.0;
/// A tile's corners, and its heading's height.
const TILE_R: f32 = 18.0;
const HEAD_H: f32 = 40.0;
/// A dock item's size.
const ITEM: f32 = 42.0;
/// The conversations' rows, and their group headings.
const ROW_H: f32 = 52.0;
const GROUP_H: f32 = 28.0;
/// A filter chip's height.
const CHIP_H: f32 = 24.0;
/// The conversations' search field (focused by Ctrl+F, the sidebar's
/// Conversations and the dock's).
pub(crate) const SEARCH_ID: &str = "home-chat-search";

/// The room the dock leaves above it for the desktop and the menu.
pub(crate) fn above_dock(rect: Rect) -> Rect {
    Rect::from_min_max(
        rect.min,
        egui::pos2(rect.right(), rect.bottom() - DOCK_ROOM + GAP),
    )
}

/// Puts the keyboard in the conversations' search field, as it is next
/// drawn (it may not be drawn this frame: another page is showing, or the
/// menu, and egui forgets a focus asked for a widget it didn't see).
pub(crate) fn focus_search(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(focus_id(), true));
    ctx.request_repaint();
}

fn focus_id() -> Id {
    Id::unique("desktop-focus-search")
}

/// The conversations' search field has the keyboard.
pub(crate) fn searching(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(Id::unique(SEARCH_ID)))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tile {
    Chats,
    Clock,
    Calendar,
    Overview,
    Usage,
}

/// Which conversations the list shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Show {
    #[default]
    All,
    Working,
    Paused,
}

/// The list's filter, besides the search: a chip and a calendar day. Kept
/// in egui's memory, for the session.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
struct Filter {
    show: Show,
    day: Option<NaiveDate>,
}

fn filter_id() -> Id {
    Id::unique("desktop-filter")
}

impl Filter {
    fn get(ctx: &egui::Context) -> Filter {
        ctx.data(|d| d.get_temp(filter_id())).unwrap_or_default()
    }

    fn set(self, ctx: &egui::Context) {
        ctx.data_mut(|d| d.insert_temp(filter_id(), self));
    }

    fn keeps(&self, r: &ChatRow) -> bool {
        let shown = match self.show {
            Show::All => true,
            Show::Working => r.busy,
            Show::Paused => r.paused,
        };
        shown && self.day.is_none_or(|d| day_of(r.at) == Some(d))
    }
}

/// The local date of a time (ms since the epoch).
fn day_of(ms: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&Local).date_naive())
}

const WIDE: f32 = 900.0;
const MEDIUM: f32 = 580.0;

/// The grid scrolls rather than compressing its tiles below these heights.
fn grid_height(width: f32) -> f32 {
    if width >= WIDE {
        560.0
    } else if width >= MEDIUM {
        230.0 + 460.0 + 230.0 + 2.0 * GAP
    } else {
        210.0 + 460.0 + 270.0 + 230.0 + 220.0 + 4.0 * GAP
    }
}

/// Where the tiles go in `area` (at least `grid_height` tall).
fn layout(area: Rect) -> Vec<(Tile, Rect)> {
    let cols = |x: Rangef, weights: &[f32], y: Rangef| -> Vec<Rect> {
        let total: f32 = weights.iter().sum();
        let room = x.span() - GAP * (weights.len() - 1) as f32;
        let mut left = x.min;
        weights
            .iter()
            .map(|w| {
                let r = Rect::from_x_y_ranges(left..=left + room * w / total, y);
                left = r.right() + GAP;
                r
            })
            .collect()
    };
    let rows = |y: Rangef, shares: &[f32]| -> Vec<Rangef> {
        let total: f32 = shares.iter().sum();
        let room = y.span() - GAP * (shares.len() - 1) as f32;
        let mut top = y.min;
        shares
            .iter()
            .map(|s| {
                let r = Rangef::new(top, top + room * s / total);
                top = r.max + GAP;
                r
            })
            .collect()
    };
    use Tile::*;
    if area.width() >= WIDE {
        // The conversations down the left; the clock, the calendar and the
        // overview, and the usage on the right.
        let c = cols(area.x_range(), &[1.25, 1.0], area.y_range());
        let r = rows(c[1].y_range(), &[0.30, 0.40, 0.30]);
        let mid = cols(c[1].x_range(), &[1.0, 1.0], r[1]);
        vec![
            (Chats, c[0]),
            (Clock, Rect::from_x_y_ranges(c[1].x_range(), r[0])),
            (Calendar, mid[0]),
            (Overview, mid[1]),
            (Usage, Rect::from_x_y_ranges(c[1].x_range(), r[2])),
        ]
    } else if area.width() >= MEDIUM {
        let r = rows(area.y_range(), &[230.0, 460.0, 230.0]);
        let a = cols(area.x_range(), &[1.24, 1.0], r[0]);
        let c = cols(area.x_range(), &[1.0, 1.0], r[2]);
        vec![
            (Clock, a[0]),
            (Calendar, a[1]),
            (Chats, Rect::from_x_y_ranges(area.x_range(), r[1])),
            (Overview, c[0]),
            (Usage, c[1]),
        ]
    } else {
        [Clock, Chats, Calendar, Overview, Usage]
            .into_iter()
            .zip(rows(area.y_range(), &[210.0, 460.0, 270.0, 230.0, 220.0]))
            .map(|(tile, y)| (tile, Rect::from_x_y_ranges(area.x_range(), y)))
            .collect()
    }
}

/// What the conversations' list asked for.
#[derive(Default)]
struct RowAct {
    open: Option<String>,
    rename: Option<String>,
    fork: Option<String>,
    delete: Option<String>,
    clear_scratch: Option<String>,
}

impl RowAct {
    fn merge(&mut self, o: RowAct) {
        self.open = self.open.take().or(o.open);
        self.rename = self.rename.take().or(o.rename);
        self.fork = self.fork.take().or(o.fork);
        self.delete = self.delete.take().or(o.delete);
        self.clear_scratch = self.clear_scratch.take().or(o.clear_scratch);
    }
}

/// An entry in the list: a group's heading, or a conversation.
enum Entry<'a> {
    Group(&'static str, usize),
    Row(&'a ChatRow),
}

/// The list's entries: those at work first, then those paused, then
/// (sorted by the latest) the rest by day; sorted by scratchpad, one group.
fn entries<'a>(rows: &'a [ChatRow], today: NaiveDate, by_day: bool) -> Vec<Entry<'a>> {
    let mut out = Vec::with_capacity(rows.len() + 6);
    let working: Vec<&ChatRow> = rows.iter().filter(|r| r.busy).collect();
    if !working.is_empty() {
        out.push(Entry::Group("Working", working.len()));
        out.extend(working.into_iter().map(Entry::Row));
    }
    let paused: Vec<&ChatRow> = rows.iter().filter(|r| r.paused && !r.busy).collect();
    if !paused.is_empty() {
        out.push(Entry::Group("Paused", paused.len()));
        out.extend(paused.into_iter().map(Entry::Row));
    }
    let rest: Vec<&ChatRow> = rows.iter().filter(|r| !(r.busy || r.paused)).collect();
    if !by_day {
        if !rest.is_empty() && !out.is_empty() {
            out.push(Entry::Group("By scratchpad size", rest.len()));
        }
        out.extend(rest.into_iter().map(Entry::Row));
        return out;
    }
    let group = |r: &ChatRow| match day_of(r.at).map(|d| (today - d).num_days()) {
        Some(..=0) => "Today",
        Some(1) => "Yesterday",
        Some(2..=6) => "This week",
        _ => "Earlier",
    };
    let mut i = 0;
    while i < rest.len() {
        let g = group(rest[i]);
        let n = rest[i..].iter().take_while(|r| group(r) == g).count();
        out.push(Entry::Group(g, n));
        out.extend(rest[i..i + n].iter().copied().map(Entry::Row));
        i += n;
    }
    out
}

fn entry_h(e: &Entry) -> f32 {
    match e {
        Entry::Group(..) => GROUP_H,
        Entry::Row(_) => ROW_H,
    }
}

/// A conversation's color: one of the marks, by its folder.
fn folder_mark(folder: &str) -> Color32 {
    let name = folder.split(',').next().unwrap_or(folder);
    let h = name
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    hex(MARKS[(h % MARKS.len() as u32) as usize])
}

impl App {
    /// The desktop's tiles, in `rect` (the pane, above the dock).
    pub(crate) fn desktop(&mut self, ui: &mut Ui, t: &Theme, rect: Rect) {
        let area = above_dock(rect).shrink(GAP + 6.0);
        if !area.is_positive() {
            return;
        }
        let h = grid_height(area.width());
        if area.height() >= h {
            self.desktop_tiles(ui, t, area);
        } else {
            ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
                ui.shrink_clip_rect(area);
                ScrollArea::vertical()
                    .id_salt("desktop-tiles")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let room = ui.available_rect_before_wrap();
                        let width = (room.width() - 8.0).max(1.0);
                        let (grid, _) =
                            ui.allocate_exact_size(vec2(width, grid_height(width)), Sense::hover());
                        self.desktop_tiles(ui, t, grid);
                    });
            });
        }
    }

    fn desktop_tiles(&mut self, ui: &mut Ui, t: &Theme, area: Rect) {
        let rows = self.chat_rows();
        let mut filter = Filter::get(ui.ctx());
        let was = filter;
        // The seconds tick: the next frame when the next one starts.
        let now = Local::now();
        ui.ctx().request_repaint_after(Duration::from_millis(
            1000 - now.timestamp_subsec_millis().min(999) as u64,
        ));
        // Each tile on whole pixels: the shares and gaps divide the area in
        // fractions, which would put a tile's one-pixel edges between rows.
        let ppp = ui.ctx().pixels_per_point();
        let px = |v: f32| (v * ppp).round() / ppp;
        let tiles = layout(area).into_iter().map(|(tile, r)| {
            (tile, Rect::from_min_max(egui::pos2(px(r.left()), px(r.top())), egui::pos2(px(r.right()), px(r.bottom()))))
        });
        for (tile, r) in tiles {
            if !ui.is_rect_visible(r) {
                continue;
            }
            // A tile takes the clicks on it, so they don't reach the wallpaper.
            let resp = ui.interact(r, Id::unique(("desktop-tile", tile as u8)), Sense::click());
            let lit = ui.ctx().animate_bool_with_time(
                Id::unique(("desktop-tile-lit", tile as u8)),
                resp.contains_pointer(),
                0.18,
            );
            let (title, mark) = match tile {
                Tile::Chats => ("CONVERSATIONS", MARKS[0]),
                Tile::Clock => ("LOCAL TIME", MARKS[0]),
                Tile::Calendar => ("CALENDAR", MARKS[2]),
                Tile::Overview => ("OVERVIEW", MARKS[1]),
                Tile::Usage => ("USAGE", MARKS[3]),
            };
            paint_tile(ui.painter(), t, r, title, hex(mark), lit);
            let body = Rect::from_min_max(
                egui::pos2(r.left() + 18.0, r.top() + HEAD_H),
                egui::pos2(r.right() - 18.0, r.bottom() - 16.0),
            );
            match tile {
                Tile::Chats => {
                    if self.chats_tile(ui, t, r, body, &rows, &mut filter) {
                        return;
                    }
                }
                Tile::Clock => clock(ui.painter(), t, body, &now),
                Tile::Calendar => {
                    if let Some(d) = calendar(ui, t, body, &now, &rows, filter.day) {
                        filter.day = (filter.day != Some(d)).then_some(d);
                    }
                }
                Tile::Overview => {
                    if let Some(s) = overview(ui, t, body, &rows, self.settings.tabs.len(), filter.show) {
                        filter.show = s;
                        filter.day = None;
                    }
                }
                Tile::Usage => {
                    if usage_view::preview(ui, t, body, &mut self.usage) {
                        self.page = Page::Usage;
                    }
                }
            }
        }
        if filter != was {
            filter.set(ui.ctx());
        }
    }

    /// The conversations' tile: New at the heading's right; the search
    /// field and the sort, the chips, and the list. True when it moved off
    /// Home.
    fn chats_tile(
        &mut self,
        ui: &mut Ui,
        t: &Theme,
        tile: Rect,
        body: Rect,
        all: &[ChatRow],
        filter: &mut Filter,
    ) -> bool {
        // + New, at the heading's right.
        let new = Rect::from_min_size(
            egui::pos2(tile.right() - 18.0 - 70.0, tile.top() + 9.0),
            vec2(70.0, 26.0),
        );
        let resp = ui
            .interact(new, Id::unique("desktop-new"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("New conversation (Ctrl+T)");
        let lift = ui.ctx().animate_bool_with_time(Id::unique("desktop-new-lit"), resp.hovered(), 0.12);
        let p = ui.painter();
        p.rect_filled(new, 13.0, mix(mix(t.panel, t.accent, 0.30), t.accent, lift));
        gravity::edge_light(p, new, 13.0, 0.9 + 0.6 * lift);
        p.text(
            new.center(),
            Align2::CENTER_CENTER,
            "+  New",
            FontId::proportional(12.5),
            t.ink,
        );
        if resp.clicked() {
            self.screen = Screen::New;
            self.page = Page::Chats;
            ui.memory_mut(|m| m.request_focus(Id::unique("composer")));
            return true;
        }
        // How many there are, after the title.
        let n_all = all.len();
        ui.painter().text(
            egui::pos2(tile.left() + 34.0 + 118.0, tile.top() + HEAD_H / 2.0 + 2.0),
            Align2::LEFT_CENTER,
            n_all.to_string(),
            FontId::proportional(11.0),
            t.ink4,
        );

        // The search field and the sort.
        let mut rows = all.to_vec();
        let sort = home::ChatSort::from_setting(&self.settings.chat_sort);
        if sort == home::ChatSort::Scratchpad {
            let scan = crate::scratch::scan(Duration::from_secs(15), ui.ctx());
            if scan.at.is_some() {
                let sizes: HashMap<&str, u64> = scan
                    .pads
                    .iter()
                    .flat_map(|p| p.owners.iter().map(|o| (o.as_str(), p.bytes)))
                    .collect();
                home::sort_by_scratch(&mut rows, |id| sizes.get(id).copied());
            }
        }
        let top = body.top() + 2.0;
        let sort_w = home::SORT_W.min(body.width() * 0.34);
        let sort_rect = Rect::from_min_max(
            egui::pos2(body.right() - sort_w, top),
            egui::pos2(body.right(), top + home::SEARCH_H),
        );
        let field = Rect::from_min_max(
            egui::pos2(body.left(), top),
            egui::pos2(sort_rect.left() - 8.0, top + home::SEARCH_H),
        );
        // Not in a frame with the pointer pressed or let go (the click that
        // asked for it): the field would let the focus go again, as for a
        // click outside it.
        let pointer = ui.input(|i| i.pointer.any_down() || i.pointer.any_pressed() || i.pointer.any_released());
        if pointer {
            if ui.data(|d| d.get_temp::<bool>(focus_id())).unwrap_or(false) {
                ui.ctx().request_repaint();
            }
        } else if ui.data_mut(|d| d.remove_temp::<bool>(focus_id())).unwrap_or(false) {
            ui.memory_mut(|m| m.request_focus(Id::unique(SEARCH_ID)));
        }
        let enter = home::search_field(ui, t, field, &mut self.chat_filter);
        if let Some(pick) = home::sort_button(ui, t, sort_rect, sort).filter(|s| *s != sort) {
            self.settings.chat_sort = pick.setting().into();
            store::save_settings(&self.settings);
        }

        // The chips.
        let chips_y = field.bottom() + 10.0;
        let working = all.iter().filter(|r| r.busy).count();
        let paused = all.iter().filter(|r| r.paused).count();
        let mut x = body.left();
        for (show, label, n, mark) in [
            (Show::All, "All", n_all, t.ink2),
            (Show::Working, "Working", working, hex(MARKS[1])),
            (Show::Paused, "Paused", paused, hex(MARKS[2])),
        ] {
            let text = format!("{label}  {n}");
            if chip(ui, t, &mut x, chips_y, &text, mark, filter.show == show, false, ("chip", show as u8)) {
                filter.show = show;
            }
        }
        if let Some(d) = filter.day {
            let text = d.format("%b %-d").to_string();
            if chip(ui, t, &mut x, chips_y, &text, t.accent, true, true, ("chip-day", 0)) {
                filter.day = None;
            }
        }

        // The list, faded in as the filter or the search changes.
        let list = Rect::from_min_max(
            egui::pos2(body.left() - 8.0, chips_y + CHIP_H + 10.0),
            egui::pos2(body.right() + 10.0, tile.bottom() - 12.0),
        );
        let shown: Vec<ChatRow> = home::filter_rows(rows, &self.chat_filter)
            .into_iter()
            .filter(|r| filter.keeps(r))
            .collect();
        let key = (filter.show as u8, filter.day, self.chat_filter.clone());
        let key_id = Id::unique("desktop-list-key");
        let now = ui.input(|i| i.time);
        let since = ui.data_mut(|d| {
            let last = d.get_temp::<(u8, Option<NaiveDate>, String)>(key_id);
            if last.as_ref() != Some(&key) {
                d.insert_temp(key_id, key);
                d.insert_temp(key_id.with("at"), now);
            }
            d.get_temp::<f64>(key_id.with("at")).unwrap_or(0.0)
        });
        let fade = (((now - since) / 0.22) as f32).clamp(0.0, 1.0);
        if fade < 1.0 {
            ui.ctx().request_repaint();
        }
        let ease = 1.0 - (1.0 - fade).powi(3);
        if shown.is_empty() {
            let any = !self.chat_filter.trim().is_empty() || *filter != Filter::default();
            if self.empty_list(ui, t, list, any) {
                self.chat_filter.clear();
                *filter = Filter::default();
            }
        } else {
            let today = Local::now().date_naive();
            let items = entries(&shown, today, sort == home::ChatSort::Recent);
            let act = ui
                .scope_builder(UiBuilder::new().max_rect(list), |ui| {
                    ui.set_opacity(ease);
                    chat_entries(ui, t, &items, (1.0 - ease) * 8.0)
                })
                .inner;
            if let Some(id) = act.rename {
                self.start_rename(&id);
            }
            if let Some(id) = act.fork {
                self.fork_conversation(&id, None, ui.ctx());
            }
            if let Some(id) = act.delete {
                self.deleting_chat = Some(id);
            }
            if let Some(id) = act.clear_scratch {
                std::thread::spawn(move || crate::scratch::clear_conversation(&id));
            }
            // Enter in the search field opens the first match.
            let first = (enter && !self.chat_filter.trim().is_empty())
                .then(|| items.iter().find_map(|e| match e {
                    Entry::Row(r) => Some(r.id.clone()),
                    _ => None,
                }))
                .flatten();
            if let Some(id) = act.open.or(first) {
                self.open(id);
                return true;
            }
        }
        false
    }

    /// An empty list in `rect`: the arch, faint, and a note; with a filter
    /// or search on, a button clearing them (whether it was clicked).
    fn empty_list(&mut self, ui: &mut Ui, t: &Theme, rect: Rect, filtered: bool) -> bool {
        let c = rect.center() - vec2(0.0, 24.0);
        let logo = Rect::from_center_size(c, vec2(56.0, 56.0));
        gravity::paint_logo(ui.painter(), logo, Some(t.ink.gamma_multiply(0.10)));
        ui.painter().text(
            egui::pos2(c.x, logo.bottom() + 8.0),
            Align2::CENTER_TOP,
            if filtered { "No matching conversations" } else { "No conversations yet" },
            FontId::proportional(13.0),
            t.ink3,
        );
        if !filtered {
            return false;
        }
        let b = Rect::from_center_size(egui::pos2(c.x, logo.bottom() + 46.0), vec2(112.0, 26.0));
        let resp = ui
            .interact(b, Id::unique("desktop-clear-filter"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let p = ui.painter();
        p.rect_filled(b, 13.0, if resp.hovered() { t.panel2 } else { mix(t.panel, t.ink, 0.04) });
        gravity::edge_light(p, b, 13.0, 1.0);
        p.text(b.center(), Align2::CENTER_CENTER, "Clear filters", FontId::proportional(12.5), t.ink2);
        resp.clicked()
    }

    /// The dock at the bottom of `rect`: the conversations' search, Home's
    /// menu on its other lists, New conversation and Settings. Gives its
    /// rectangle.
    pub(crate) fn dock(&mut self, ui: &mut Ui, t: &Theme, rect: Rect) -> Rect {
        enum Act {
            Search,
            List(&'static str),
            New,
            Settings,
        }
        let items: [(&str, &str, Act); 6] = [
            ("\u{f086}", "Search conversations (Ctrl+F)", Act::Search),
            ("\u{f0c2}", "Cloud", Act::List(home::CLOUD_TAB)),
            ("\u{f1e6}", "Protocols", Act::List(crate::mcp_view::TAB)),
            ("\u{f12e}", "Plugins", Act::List(crate::plugins_view::TAB)),
            ("\u{f067}", "New conversation (Ctrl+T)", Act::New),
            (crate::composer::GEAR_ICON, "Settings", Act::Settings),
        ];
        let (pad, gap, sep) = (8.0, 6.0, 18.0);
        let item_size = ITEM.min(
            ((rect.width() - 2.0 * pad - 4.0 * gap - sep - 16.0) / items.len() as f32).max(12.0),
        );
        let w =
            items.len() as f32 * item_size + (items.len() - 1) as f32 * gap + 2.0 * pad + sep - gap;
        let dock = Rect::from_center_size(
            egui::pos2(rect.center().x, rect.bottom() - 14.0 - DOCK_H / 2.0),
            vec2(w, DOCK_H),
        );
        let _ = ui.interact(dock, Id::unique("desktop-dock"), Sense::click());
        let p = ui.painter().clone();
        p.add(
            egui::Shadow {
                offset: [0, 8],
                blur: 28,
                spread: 0,
                color: Color32::from_black_alpha(130),
            }
            .as_shape(dock, 20),
        );
        let sh = gravity::shell();
        p.rect_filled(
            dock,
            20.0,
            Color32::from_rgba_unmultiplied(sh.r(), sh.g(), sh.b(), 236),
        );
        gravity::edge_light(&p, dock, 20.0, 1.2);
        let searching = !self.home_menu && searching(ui.ctx());
        let mut x = dock.left() + pad;
        let mut act = None;
        for (i, (icon, tip, a)) in items.into_iter().enumerate() {
            if i == 4 {
                p.vline(
                    x - gap + sep / 2.0,
                    dock.center().y - 12.0..=dock.center().y + 12.0,
                    Stroke::new(1.0, t.line),
                );
                x += sep - gap;
            }
            let item = Rect::from_min_size(
                egui::pos2(x, dock.center().y - item_size / 2.0),
                vec2(item_size, item_size),
            );
            x = item.right() + gap;
            let on = match a {
                Act::Search => searching,
                Act::List(id) => self.home_menu && self.home_list == id,
                _ => false,
            };
            let resp = ui
                .interact(item, Id::unique(("dock", i)), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(tip);
            let lift =
                ui.ctx()
                    .animate_bool_with_time(Id::unique(("dock-lift", i)), resp.hovered(), 0.12);
            let face = item.translate(vec2(0.0, -3.0 * lift));
            let mark = hex(MARKS[i % MARKS.len()]);
            let fill = if on {
                mix(sh, mark, 0.22)
            } else {
                mix(sh, t.ink, 0.05 + 0.07 * lift)
            };
            p.rect_filled(face, 12.0, fill);
            gravity::edge_light(&p, face, 12.0, 0.7 + 0.6 * lift);
            if on || lift > 0.0 {
                p.rect_stroke(
                    face,
                    12.0,
                    Stroke::new(1.0, mark.gamma_multiply(if on { 0.7 } else { 0.4 * lift })),
                    egui::StrokeKind::Inside,
                );
            }
            let ink = if on {
                t.ink
            } else {
                t.ink2.lerp_to_gamma(mark, lift)
            };
            p.text(
                face.center(),
                Align2::CENTER_CENTER,
                icon,
                markdown::icon_font(16.0),
                ink,
            );
            if on {
                p.circle_filled(egui::pos2(item.center().x, dock.bottom() - 4.0), 2.0, mark);
            }
            if resp.clicked() {
                act = Some((a, on));
            }
        }
        match act {
            Some((Act::Search, _)) => {
                self.home_menu = false;
                focus_search(ui.ctx());
            }
            Some((Act::List(id), on)) => {
                self.home_menu = !on;
                self.home_list = id.into();
            }
            Some((Act::New, _)) => {
                self.screen = Screen::New;
                self.page = Page::Chats;
                ui.memory_mut(|m| m.request_focus(Id::unique("composer")));
            }
            Some((Act::Settings, _)) => {
                self.settings_from = Some(Page::Home);
                self.page = Page::Settings;
            }
            None => {}
        }
        dock
    }
}

/// A filter chip at `x` (moved past it), `y` its top: `text` after a dot in
/// `mark`, lit when `on`; with `close`, an x after it. Whether it was
/// clicked.
#[allow(clippy::too_many_arguments)]
fn chip(
    ui: &mut Ui,
    t: &Theme,
    x: &mut f32,
    y: f32,
    text: &str,
    mark: Color32,
    on: bool,
    close: bool,
    salt: (&str, u8),
) -> bool {
    let font = FontId::proportional(12.0);
    let tw = crate::widgets::text_width(ui, text, &font);
    let w = 26.0 + tw + if close { 18.0 } else { 0.0 } + 4.0;
    let rect = Rect::from_min_size(egui::pos2(*x, y), vec2(w, CHIP_H));
    *x = rect.right() + 6.0;
    let resp = ui
        .interact(rect, Id::unique(("desktop", salt)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let lit = ui.ctx().animate_bool_with_time(Id::unique(("desktop-lit", salt)), resp.hovered(), 0.12);
    let sel = ui.ctx().animate_bool_with_time(Id::unique(("desktop-on", salt)), on, 0.16);
    let p = ui.painter();
    let fill = Color32::from_white_alpha((10.0 + 10.0 * lit) as u8)
        .lerp_to_gamma(mark.gamma_multiply(0.22), sel);
    p.rect_filled(rect, CHIP_H / 2.0, fill);
    gravity::edge_light(p, rect, CHIP_H / 2.0, 0.6 + 0.5 * lit + 0.4 * sel);
    if sel > 0.0 {
        p.rect_stroke(
            rect,
            CHIP_H / 2.0,
            Stroke::new(1.0, mark.gamma_multiply(0.55 * sel)),
            egui::StrokeKind::Inside,
        );
    }
    p.circle_filled(egui::pos2(rect.left() + 13.0, rect.center().y), 3.0, mark);
    let ink = t.ink2.lerp_to_gamma(t.ink, lit.max(sel));
    p.text(
        egui::pos2(rect.left() + 22.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        ink,
    );
    if close {
        let xr = Rect::from_center_size(egui::pos2(rect.right() - 13.0, rect.center().y), vec2(9.0, 9.0));
        crate::widgets::draw_x(p, xr, ink);
    }
    resp.clicked()
}

/// The list's entries in a scroll area, only those in view laid out, moved
/// `shift` down (as they fade in).
fn chat_entries(ui: &mut Ui, t: &Theme, items: &[Entry], shift: f32) -> RowAct {
    let mut act = RowAct::default();
    let total: f32 = items.iter().map(entry_h).sum();
    ScrollArea::vertical()
        .id_salt("desktop-chats")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, view| {
            let w = ui.available_width();
            let (all, _) = ui.allocate_exact_size(vec2(w, total + 6.0), Sense::hover());
            let mut y = 0.0;
            for (i, e) in items.iter().enumerate() {
                let h = entry_h(e);
                if y + h >= view.top() && y <= view.bottom() {
                    let rect = Rect::from_min_size(
                        all.min + vec2(0.0, y + shift),
                        vec2(w - home::BAR_ROOM, h),
                    );
                    match e {
                        Entry::Group(label, n) => group_head(ui.painter(), t, rect, label, *n, i == 0),
                        Entry::Row(r) => act.merge(chat_row(ui, t, rect, r)),
                    }
                }
                y += h;
            }
        });
    act
}

/// A group's heading: its name spaced out and its count, a hairline after.
fn group_head(p: &egui::Painter, t: &Theme, rect: Rect, label: &str, n: usize, first: bool) {
    let y = rect.center().y + if first { -2.0 } else { 3.0 };
    let x = rect.left() + 10.0;
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &label.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(10.5),
            color: t.ink3,
            extra_letter_spacing: 1.0,
            ..Default::default()
        },
    );
    job.append(
        &format!("   {n}"),
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(10.5),
            color: t.ink4,
            ..Default::default()
        },
    );
    let g = p.layout_job(job);
    let w = g.size().x;
    p.galley(egui::pos2(x, y - g.size().y / 2.0), g, t.ink3);
    if x + w + 12.0 < rect.right() - 10.0 {
        p.hline(
            x + w + 12.0..=rect.right() - 10.0,
            y,
            Stroke::new(1.0, Color32::from_white_alpha(10)),
        );
    }
}

/// A conversation's row: its folder's initial in its color (and its dot
/// when at work or paused), the title and the folder under it, and on the
/// right when it was last active (or that it is working or paused, or its
/// scratchpad's size). Under the pointer it lights, an accent edge grows at
/// its left and its quick actions fade in; a right click has them too.
fn chat_row(ui: &mut Ui, t: &Theme, rect: Rect, r: &ChatRow) -> RowAct {
    let mut act = RowAct::default();
    let resp = ui
        .interact(rect, Id::unique(("desktop-chat", &r.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let inside = ui.rect_contains_pointer(rect);
    let h = ui
        .ctx()
        .animate_bool_with_time(Id::unique(("desktop-chat-lit", &r.id)), inside, 0.14);
    let p = ui.painter().clone();
    let face = rect.shrink2(vec2(2.0, 2.0));
    if h > 0.0 {
        p.rect_filled(face, 12.0, Color32::from_white_alpha((12.0 * h) as u8));
        gravity::edge_light(&p, face, 12.0, 0.55 * h);
        let bar = Rect::from_center_size(
            egui::pos2(face.left() + 2.5, face.center().y),
            vec2(3.0, 20.0 * h),
        );
        p.rect_filled(bar, 1.5, t.accent.gamma_multiply(h));
    }
    let cy = face.center().y;
    // The folder's initial, in its color.
    let mark = folder_mark(&r.folder);
    let glyph = Rect::from_center_size(egui::pos2(face.left() + 26.0, cy), vec2(30.0, 30.0));
    p.rect_filled(glyph, 9.0, mark.gamma_multiply(0.16 + 0.08 * h));
    p.rect_stroke(
        glyph,
        9.0,
        Stroke::new(1.0, mark.gamma_multiply(0.30 + 0.25 * h)),
        egui::StrokeKind::Inside,
    );
    let initial: String = r
        .folder
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or("·".into(), |c| c.to_uppercase().collect());
    p.text(
        glyph.center(),
        Align2::CENTER_CENTER,
        initial,
        FontId::new(13.0, egui::FontFamily::Name("bold".into())),
        mark.lerp_to_gamma(t.ink, 0.25),
    );
    if r.busy || r.paused {
        let at = glyph.right_bottom() - vec2(1.0, 1.0);
        p.circle_filled(at, 5.5, t.panel);
        crate::widgets::state_dot(&p, t, at, r.paused);
    }
    // The quick actions, right to left.
    let mut acts: Vec<(&str, &str, u8)> = vec![("\u{f1f8}", "Delete conversation", 2)];
    if r.renamable {
        acts.push(("\u{f126}", "Fork conversation", 1));
        acts.push(("\u{f040}", "Rename\u{2026}", 0));
    }
    let mut bx = face.right() - 8.0;
    let mut first_btn = face.right();
    for (icon, tip, which) in acts {
        let b = Rect::from_min_size(egui::pos2(bx - 28.0, cy - 14.0), vec2(28.0, 28.0));
        bx = b.left() - 4.0;
        first_btn = b.left();
        if h < 0.05 {
            continue;
        }
        let br = ui
            .interact(b, Id::unique(("desktop-chat-act", &r.id, which)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tip);
        let danger = which == 2;
        if br.hovered() {
            p.rect_filled(
                b,
                8.0,
                if danger { t.error.gamma_multiply(0.22) } else { Color32::from_white_alpha(18) },
            );
            gravity::edge_light(&p, b, 8.0, 0.8);
        }
        let ink = match (br.hovered(), danger) {
            (true, true) => t.error,
            (true, false) => t.ink,
            _ => t.ink3,
        };
        p.text(b.center(), Align2::CENTER_CENTER, icon, markdown::icon_font(13.0), ink.gamma_multiply(h));
        if br.clicked() {
            let id = Some(r.id.clone());
            match which {
                0 => act.rename = id,
                1 => act.fork = id,
                _ => act.delete = id,
            }
        }
    }
    // What is on the right: when it was last active, or what it is doing.
    let (right_text, right_ink) = match (r.busy, r.paused, r.scratch) {
        (true, _, _) => ("Working".to_string(), hex(MARKS[1])),
        (_, true, _) => ("Paused".to_string(), hex(MARKS[2])),
        (_, _, Some(n)) => (crate::scratch::fmt_bytes(n), if n > 0 { t.ink2 } else { t.ink4 }),
        _ => (ago(r.at), t.ink3),
    };
    let right = face.right() - 12.0;
    p.text(
        egui::pos2(right + 6.0 * h, cy - 8.0),
        Align2::RIGHT_CENTER,
        &right_text,
        FontId::proportional(11.5),
        right_ink.gamma_multiply(1.0 - h),
    );
    p.text(
        egui::pos2(right + 6.0 * h, cy + 10.0),
        Align2::RIGHT_CENTER,
        mdy(r.at),
        FontId::proportional(10.5),
        t.ink4.gamma_multiply(1.0 - h),
    );
    // The title and the folder.
    let x = glyph.right() + 12.0;
    let end = (right - 64.0).max(x + 40.0).min(if h > 0.05 { first_btn - 8.0 } else { f32::INFINITY });
    let title = FontId::proportional(13.5);
    let small = FontId::proportional(11.5);
    let shown = crate::widgets::clip_width(ui, &r.title, &title, end - x);
    let cut = shown != r.title;
    p.text(
        egui::pos2(x, cy - 8.0),
        Align2::LEFT_CENTER,
        shown,
        title,
        home::row_title_color(t, h > 0.5),
    );
    p.text(
        egui::pos2(x, cy + 10.0),
        Align2::LEFT_CENTER,
        crate::widgets::clip_width(ui, &r.folder, &small, end - x),
        small,
        t.ink3,
    );
    // The whole title, only when it is cut.
    let resp = if cut { resp.on_hover_text(&r.title) } else { resp };
    resp.context_menu(|ui| {
        if r.renamable && ui.button("Rename\u{2026}").clicked() {
            act.rename = Some(r.id.clone());
            ui.close();
        }
        if r.renamable
            && ui
                .button("Fork conversation")
                .on_hover_text("A new conversation with this one's transcript and notes, and no running work. Files are shared.")
                .clicked()
        {
            act.fork = Some(r.id.clone());
            ui.close();
        }
        ui.separator();
        let size = crate::scratch::scan(Duration::from_secs(15), ui.ctx()).of(&r.id);
        let label = match size {
            Some(n) if n > 0 => format!("Clear scratchpad ({})", crate::scratch::fmt_bytes(n)),
            _ => "Clear scratchpad".to_string(),
        };
        if ui
            .add_enabled(size.is_some_and(|n| n > 0), Button::new(label))
            .on_hover_text("Deletes the files in the session's .oak/tmp folder.")
            .on_disabled_hover_text("Its scratchpad is empty.")
            .clicked()
        {
            act.clear_scratch = Some(r.id.clone());
            ui.close();
        }
        if ui
            .button("Delete conversation")
            .on_hover_text("Removes the conversation and its transcript file.")
            .clicked()
        {
            act.delete = Some(r.id.clone());
            ui.close();
        }
    });
    if resp.clicked() {
        act.open = Some(r.id.clone());
    }
    act
}

/// A tile: a soft shadow, the panel a little see-through over the
/// wallpaper, lit along its edges (brighter by `lit`, under the pointer),
/// and its heading (a dot in `mark`, the title spaced out).
fn paint_tile(p: &egui::Painter, t: &Theme, r: Rect, title: &str, mark: Color32, lit: f32) {
    p.add(
        egui::Shadow {
            offset: [0, 6],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(90),
        }
        .as_shape(r, TILE_R as u8),
    );
    p.rect_filled(
        r,
        TILE_R,
        Color32::from_rgba_unmultiplied(t.panel.r(), t.panel.g(), t.panel.b(), 222),
    );
    gravity::edge_light(p, r, TILE_R, 1.0 + 0.45 * lit);
    let y = r.top() + HEAD_H / 2.0 + 2.0;
    p.circle_filled(egui::pos2(r.left() + 22.0, y), 3.5, mark);
    p.circle_filled(
        egui::pos2(r.left() + 22.0, y),
        7.0 + 1.5 * lit,
        mark.gamma_multiply(0.15 + 0.1 * lit),
    );
    spaced(p, egui::pos2(r.left() + 34.0, y), title, 11.0, t.ink3.lerp_to_gamma(t.ink2, lit));
}

/// `text` spaced out, its left middle at `at`.
fn spaced(p: &egui::Painter, at: egui::Pos2, text: &str, size: f32, color: Color32) {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(size),
            color,
            extra_letter_spacing: 1.2,
            ..Default::default()
        },
    );
    let galley = p.layout_job(job);
    let pos = at - vec2(0.0, galley.size().y / 2.0);
    p.galley(pos, galley, color);
}

/// "Good morning" (5 to 12), "Good afternoon" (to 18), "Good evening".
fn greeting(hour: u32) -> &'static str {
    match hour {
        5..12 => "Good morning",
        12..18 => "Good afternoon",
        _ => "Good evening",
    }
}

/// The clock: the greeting, the time (hours and minutes, large, the AM or
/// PM beside them) and the date on the left; on the right, when there is
/// room, a face: the minute and hour hands, and a ring the seconds fill in
/// the logo's colors.
fn clock(p: &egui::Painter, t: &Theme, body: Rect, now: &chrono::DateTime<Local>) {
    let face_d = body.height().min(body.width() * 0.42);
    let has_face = body.width() > 300.0 && face_d > 90.0;
    let text_w = if has_face {
        body.width() - face_d - 24.0
    } else {
        body.width()
    };
    let (pm, h12) = now.hour12();
    let time = format!("{h12}:{:02}", now.minute());
    let big = (body.height() * 0.40).min(text_w / 3.2).clamp(28.0, 96.0);
    let date = now.format("%A, %B %-d").to_string();
    // The block, centred down the tile.
    let block_h = 18.0 + big * 1.05 + 24.0;
    let top = body.center().y - block_h / 2.0;
    p.text(
        egui::pos2(body.left(), top),
        Align2::LEFT_TOP,
        greeting(now.hour()),
        FontId::proportional(13.0),
        t.ink3,
    );
    let r = p.text(
        egui::pos2(body.left() - big * 0.04, top + 16.0),
        Align2::LEFT_TOP,
        &time,
        FontId::proportional(big),
        t.ink,
    );
    p.text(
        egui::pos2(r.right() + 8.0, r.bottom() - big * 0.22),
        Align2::LEFT_BOTTOM,
        if pm { "PM" } else { "AM" },
        FontId::proportional((big * 0.24).max(12.0)),
        t.ink3,
    );
    p.text(
        egui::pos2(body.left(), r.bottom() + 2.0),
        Align2::LEFT_TOP,
        &date,
        FontId::proportional(14.0),
        t.ink2,
    );
    if !has_face {
        return;
    }
    let c = egui::pos2(body.right() - face_d / 2.0, body.center().y);
    let ring = face_d / 2.0 - 6.0;
    let tau = std::f32::consts::TAU;
    p.circle_stroke(c, ring, Stroke::new(5.0, Color32::from_white_alpha(12)));
    let secs = now.second() as f32;
    if secs > 0.0 {
        gravity::gradient_arc(p, c, ring, 0.0, tau * secs / 60.0, 5.0, 1.0);
    }
    // The seconds' head: a small bright dot where the ring ends.
    let a = tau * secs / 60.0;
    p.circle_filled(
        c + vec2(a.sin(), -a.cos()) * ring,
        4.0,
        gravity::brand_at(secs / 60.0),
    );
    // Ticks for the hours.
    for i in 0..12 {
        let a = tau * i as f32 / 12.0;
        let d = vec2(a.sin(), -a.cos());
        let (r0, w) = if i % 3 == 0 {
            (ring - 16.0, 2.0)
        } else {
            (ring - 12.0, 1.2)
        };
        p.line_segment([c + d * r0, c + d * (ring - 8.0)], Stroke::new(w, t.ink4));
    }
    let mins = now.minute() as f32 + secs / 60.0;
    let hours = (now.hour() % 12) as f32 + mins / 60.0;
    for (frac, len, w, color) in [
        (hours / 12.0, 0.48, 3.5, t.ink),
        (mins / 60.0, 0.72, 2.2, t.ink2),
    ] {
        let a = tau * frac;
        p.line_segment(
            [c, c + vec2(a.sin(), -a.cos()) * (ring - 8.0) * len],
            Stroke::new(w, color),
        );
    }
    p.circle_filled(c, 4.5, t.accent);
    p.circle_filled(c, 2.0, t.bg);
}

/// This month: its name, the days of the week, and the days, today in the
/// accent's circle and a dot under each day a conversation was last active.
/// A day with a dot lights under the pointer, and `picked` (the list's day)
/// has a ring; gives the day clicked.
fn calendar(
    ui: &mut Ui,
    t: &Theme,
    body: Rect,
    now: &chrono::DateTime<Local>,
    rows: &[ChatRow],
    picked: Option<NaiveDate>,
) -> Option<NaiveDate> {
    let first = now.date_naive().with_day(1).unwrap();
    let days = {
        let next = if now.month() == 12 {
            NaiveDate::from_ymd_opt(now.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1)
        };
        next.unwrap().signed_duration_since(first).num_days() as u32
    };
    let mut active: HashMap<u32, usize> = HashMap::new();
    for d in rows.iter().filter_map(|r| day_of(r.at)) {
        if d.year() == now.year() && d.month() == now.month() {
            *active.entry(d.day()).or_default() += 1;
        }
    }
    let p = ui.painter().clone();
    p.text(
        egui::pos2(body.left(), body.top() + 2.0),
        Align2::LEFT_TOP,
        now.format("%B %Y").to_string(),
        FontId::proportional(15.0),
        t.ink,
    );
    let lead = first.weekday().num_days_from_sunday();
    let weeks = (lead + days).div_ceil(7);
    let grid = Rect::from_min_max(egui::pos2(body.left(), body.top() + 30.0), body.max);
    let cw = grid.width() / 7.0;
    let ch = (grid.height() / (weeks as f32 + 1.0)).min(cw);
    let small = FontId::proportional((ch * 0.36).clamp(10.0, 13.0));
    for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
        p.text(
            egui::pos2(grid.left() + cw * (i as f32 + 0.5), grid.top() + ch * 0.5),
            Align2::CENTER_CENTER,
            *d,
            small.clone(),
            t.ink4,
        );
    }
    let num = FontId::proportional((ch * 0.40).clamp(10.5, 14.0));
    let mut clicked = None;
    for day in 1..=days {
        let cell = lead + day - 1;
        let (col, row) = (cell % 7, cell / 7);
        let c = egui::pos2(
            grid.left() + cw * (col as f32 + 0.5),
            grid.top() + ch * (row as f32 + 1.5),
        );
        let date = first.with_day(day);
        let disc = (ch * 0.42).min(cw * 0.42);
        let today = day == now.day();
        let count = active.get(&day).copied();
        let mut lit = 0.0;
        if let Some(n) = count {
            let resp = ui
                .interact(
                    Rect::from_center_size(c, vec2(disc * 2.0, disc * 2.0)),
                    Id::unique(("desktop-day", day)),
                    Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!(
                    "{n} conversation{} last active {}",
                    if n == 1 { "" } else { "s" },
                    date.map_or(String::new(), |d| d.format("%A, %B %-d").to_string())
                ));
            lit = ui.ctx().animate_bool_with_time(Id::unique(("desktop-day-lit", day)), resp.hovered(), 0.12);
            if resp.clicked() {
                clicked = date;
            }
        }
        if today {
            p.circle_filled(c, disc, t.accent);
        } else if lit > 0.0 {
            p.circle_filled(c, disc, Color32::from_white_alpha((16.0 * lit) as u8));
        }
        if picked.is_some() && picked == date {
            p.circle_stroke(c, disc + 1.5, Stroke::new(1.5, hex(MARKS[1])));
        }
        let weekend = col == 0 || col == 6;
        let color = if today {
            Color32::WHITE
        } else if lit > 0.0 {
            t.ink3.lerp_to_gamma(t.ink, lit)
        } else if day < now.day() || weekend {
            t.ink3
        } else {
            t.ink2
        };
        p.text(c, Align2::CENTER_CENTER, day.to_string(), num.clone(), color);
        if count.is_some() && !today {
            p.circle_filled(c + vec2(0.0, ch * 0.32), 1.8 + 0.6 * lit, hex(MARKS[1]));
        }
    }
    clicked
}

/// Four numbers, two by two: the conversations, those at work, those
/// paused and the tabs open, each with its color's bar; Oak's version under
/// them. A click on one of the first three shows those in the list (`on`
/// is the list's); gives which.
fn overview(ui: &mut Ui, t: &Theme, body: Rect, rows: &[ChatRow], tabs: usize, on: Show) -> Option<Show> {
    let busy = rows.iter().filter(|r| r.busy).count();
    let paused = rows.iter().filter(|r| r.paused).count();
    let cells = [
        (rows.len(), "Conversations", Some(Show::All)),
        (busy, "At work", Some(Show::Working)),
        (paused, "Paused", Some(Show::Paused)),
        (tabs, "Tabs open", None),
    ];
    let foot = 22.0;
    let grid = Rect::from_min_max(body.min, egui::pos2(body.right(), body.bottom() - foot));
    let (cw, ch) = ((grid.width() - 12.0) / 2.0, (grid.height() - 12.0) / 2.0);
    let big = FontId::new(
        (ch * 0.42).clamp(18.0, 34.0),
        egui::FontFamily::Name("bold".into()),
    );
    let p = ui.painter().clone();
    let mut pick = None;
    for (i, (n, label, show)) in cells.iter().enumerate() {
        let cell = Rect::from_min_size(
            grid.min + vec2((i % 2) as f32 * (cw + 12.0), (i / 2) as f32 * (ch + 12.0)),
            vec2(cw, ch),
        );
        // On whole pixels, for its one-pixel edges.
        let cell = Rect::from_min_max(cell.min.round(), cell.max.round());
        let mark = hex(MARKS[i]);
        let mut lit = 0.0;
        if let Some(s) = show {
            let resp = ui
                .interact(cell, Id::unique(("desktop-count", i)), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("Show {} in the list", label.to_lowercase()));
            lit = ui.ctx().animate_bool_with_time(Id::unique(("desktop-count-lit", i)), resp.hovered(), 0.12);
            if resp.clicked() {
                pick = Some(*s);
            }
        }
        let sel = *show == Some(on) && on != Show::All;
        let face = cell.translate(vec2(0.0, -2.0 * lit));
        p.rect_filled(face, 12.0, Color32::from_white_alpha((6.0 + 6.0 * lit) as u8));
        gravity::edge_light(&p, face, 12.0, 0.45 + 0.5 * lit);
        if sel {
            p.rect_stroke(face, 12.0, Stroke::new(1.0, mark.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        }
        p.rect_filled(
            Rect::from_min_size(face.min + vec2(12.0, 12.0), vec2(18.0 + 10.0 * lit, 3.0)),
            1.5,
            mark,
        );
        p.text(
            egui::pos2(face.left() + 12.0, face.center().y + 2.0),
            Align2::LEFT_CENTER,
            n.to_string(),
            big.clone(),
            t.ink,
        );
        p.text(
            egui::pos2(face.left() + 12.0, face.bottom() - 10.0),
            Align2::LEFT_BOTTOM,
            *label,
            FontId::proportional(11.5),
            t.ink3.lerp_to_gamma(t.ink2, lit),
        );
    }
    p.text(
        egui::pos2(body.left(), body.bottom()),
        Align2::LEFT_BOTTOM,
        format!("Oak {}", crate::update::VERSION),
        FontId::proportional(11.0),
        t.ink4,
    );
    pick
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each width gets its grid, and no tile overlaps another or leaves
    /// the area.
    #[test]
    fn tiles_fit_and_never_overlap() {
        for w in [1400.0, 800.0, 500.0] {
            let area = Rect::from_min_size(egui::pos2(10.0, 10.0), vec2(w, grid_height(w)));
            let tiles = layout(area);
            assert_eq!(tiles.len(), 5, "{w}");
            for (i, (_, a)) in tiles.iter().enumerate() {
                assert!(area.expand(0.5).contains_rect(*a), "{w}: {a:?}");
                for (_, b) in &tiles[i + 1..] {
                    assert!(!a.shrink(0.5).intersects(*b), "{w}: {a:?} {b:?}");
                }
            }
        }
    }

    #[test]
    fn greetings_by_the_hour() {
        assert_eq!(greeting(4), "Good evening");
        assert_eq!(greeting(9), "Good morning");
        assert_eq!(greeting(13), "Good afternoon");
        assert_eq!(greeting(21), "Good evening");
    }

    fn row(title: &str, days_ago: i64, busy: bool) -> ChatRow {
        ChatRow {
            id: title.into(),
            title: title.into(),
            folder: "oak".into(),
            at: (Local::now() - chrono::Duration::days(days_ago)).timestamp_millis(),
            busy,
            paused: false,
            renamable: true,
            scratch: None,
        }
    }

    /// What is at work comes first (then what is paused), then the rest by
    /// day, each under its heading with its count.
    #[test]
    fn entries_pin_running_then_group_by_day() {
        let rows = [row("a", 0, false), row("b", 0, true), row("c", 1, false), row("d", 3, false), row("e", 40, false)];
        let today = Local::now().date_naive();
        let heads: Vec<(&str, usize)> = entries(&rows, today, true)
            .iter()
            .filter_map(|e| match e {
                Entry::Group(g, n) => Some((*g, *n)),
                _ => None,
            })
            .collect();
        assert_eq!(heads, [("Working", 1), ("Today", 1), ("Yesterday", 1), ("This week", 1), ("Earlier", 1)]);
        let f = Filter { show: Show::Working, day: None };
        assert_eq!(rows.iter().filter(|r| f.keeps(r)).count(), 1);
        let f = Filter { show: Show::All, day: Some(today) };
        assert_eq!(rows.iter().filter(|r| f.keeps(r)).count(), 2);
    }
}
