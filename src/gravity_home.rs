//! Antigravity's Home (`gravity.rs`): a desktop. Over the dimmed wallpaper,
//! tiles in a grid: the clock (the time, the date and an analog face whose
//! ring fills with the seconds in the logo's colors), this month's calendar
//! (a dot under each day a conversation was last active), an overview
//! (conversations, at work, paused, tabs open), the recent conversations,
//! the agents at work and the usage preview. At the bottom, a dock in place
//! of the corner buttons: Home's menu on each of its lists (Conversations,
//! Cloud, Protocols, Plugins), New conversation and Settings.
//!
//! The grid reflows from three columns to two or one as the pane narrows,
//! scrolling when it is too short to show every tile. Home's menu opens
//! over the desktop above the dock.

use super::*;
use crate::gravity::{self, MARKS};
use crate::home::ChatRow;
use crate::theme::mix;
use chrono::{Datelike, Local, Timelike};
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

/// The room the dock leaves above it for the desktop and the menu.
pub(crate) fn above_dock(rect: Rect) -> Rect {
    Rect::from_min_max(
        rect.min,
        egui::pos2(rect.right(), rect.bottom() - DOCK_ROOM + GAP),
    )
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tile {
    Clock,
    Calendar,
    Overview,
    Recent,
    Agents,
    Usage,
}

const WIDE: f32 = 860.0;
const MEDIUM: f32 = 580.0;

/// The grid scrolls rather than compressing its tiles below these heights.
fn grid_height(width: f32) -> f32 {
    if width >= WIDE {
        550.0
    } else if width >= MEDIUM {
        790.0
    } else {
        6.0 * 224.0 + 5.0 * GAP
    }
}

/// Where the tiles go in `area` (at least `grid_height` tall).
fn layout(area: Rect) -> Vec<(Tile, Rect)> {
    let cols = |weights: &[f32], y: Rangef| -> Vec<Rect> {
        let total: f32 = weights.iter().sum();
        let room = area.width() - GAP * (weights.len() - 1) as f32;
        let mut x = area.left();
        weights
            .iter()
            .map(|w| {
                let r = Rect::from_x_y_ranges(x..=x + room * w / total, y);
                x = r.right() + GAP;
                r
            })
            .collect()
    };
    let rows = |shares: &[f32]| -> Vec<Rangef> {
        let total: f32 = shares.iter().sum();
        let room = area.height() - GAP * (shares.len() - 1) as f32;
        let mut y = area.top();
        shares
            .iter()
            .map(|s| {
                let r = Rangef::new(y, y + room * s / total);
                y = r.max + GAP;
                r
            })
            .collect()
    };
    use Tile::*;
    if area.width() >= WIDE {
        let r = rows(&[0.46, 0.54]);
        let a = cols(&[5.0, 3.2, 3.8], r[0]);
        let b = cols(&[5.0, 3.2, 3.8], r[1]);
        vec![
            (Clock, a[0]),
            (Calendar, a[1]),
            (Overview, a[2]),
            (Recent, b[0]),
            (Agents, b[1]),
            (Usage, b[2]),
        ]
    } else if area.width() >= MEDIUM {
        let r = rows(&[1.0, 1.10, 0.96]);
        let (a, b, c) = (
            cols(&[1.24, 1.0], r[0]),
            cols(&[1.24, 1.0], r[1]),
            cols(&[1.24, 1.0], r[2]),
        );
        vec![
            (Clock, a[0]),
            (Calendar, a[1]),
            (Recent, b[0]),
            (Agents, b[1]),
            (Overview, c[0]),
            (Usage, c[1]),
        ]
    } else {
        [Clock, Recent, Calendar, Overview, Agents, Usage]
            .into_iter()
            .zip(rows(&[1.0; 6]))
            .map(|(tile, y)| (tile, Rect::from_x_y_ranges(area.x_range(), y)))
            .collect()
    }
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
        // The seconds tick: the next frame when the next one starts.
        let now = Local::now();
        ui.ctx().request_repaint_after(Duration::from_millis(
            1000 - now.timestamp_subsec_millis().min(999) as u64,
        ));
        for (tile, r) in layout(area) {
            if !ui.is_rect_visible(r) {
                continue;
            }
            // A tile takes the clicks on it, so they don't reach the wallpaper.
            let _ = ui.interact(r, Id::unique(("desktop-tile", tile as u8)), Sense::click());
            let (title, mark) = match tile {
                Tile::Clock => ("LOCAL TIME", MARKS[0]),
                Tile::Calendar => ("CALENDAR", MARKS[2]),
                Tile::Overview => ("OVERVIEW", MARKS[1]),
                Tile::Recent => ("RECENT", MARKS[0]),
                Tile::Agents => ("ACTIVITY", MARKS[3]),
                Tile::Usage => ("USAGE", MARKS[2]),
            };
            paint_tile(ui.painter(), t, r, title, hex(mark));
            let body = Rect::from_min_max(
                egui::pos2(r.left() + 18.0, r.top() + HEAD_H),
                egui::pos2(r.right() - 18.0, r.bottom() - 16.0),
            );
            match tile {
                Tile::Clock => clock(ui.painter(), t, body, &now),
                Tile::Calendar => calendar(ui.painter(), t, body, &now, &rows),
                Tile::Overview => overview(ui.painter(), t, body, &rows, self.settings.tabs.len()),
                Tile::Recent => {
                    if self.recent(ui, t, r, body, &rows) {
                        return;
                    }
                }
                Tile::Agents => self.agents(ui, t, body, &rows),
                Tile::Usage => {
                    if usage_view::preview(ui, t, body, &mut self.usage) {
                        self.page = Page::Usage;
                    }
                }
            }
        }
    }

    /// The recent conversations, a row each, and New at the heading's
    /// right. True when it moved off Home.
    fn recent(&mut self, ui: &mut Ui, t: &Theme, tile: Rect, body: Rect, rows: &[ChatRow]) -> bool {
        let new = Rect::from_min_size(
            egui::pos2(tile.right() - 18.0 - 64.0, tile.top() + 10.0),
            vec2(64.0, 24.0),
        );
        let resp = ui
            .interact(new, Id::unique("desktop-new"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("New conversation (Ctrl+T)");
        let p = ui.painter();
        p.rect_filled(
            new,
            12.0,
            if resp.hovered() {
                t.accent
            } else {
                mix(t.panel, t.accent, 0.22)
            },
        );
        p.text(
            new.center(),
            Align2::CENTER_CENTER,
            "+ New",
            FontId::proportional(12.0),
            t.ink,
        );
        if resp.clicked() {
            self.screen = Screen::New;
            self.page = Page::Chats;
            ui.memory_mut(|m| m.request_focus(Id::unique("composer")));
            return true;
        }
        let all = Rect::from_min_size(egui::pos2(new.left() - 44.0, new.top()), vec2(36.0, 24.0));
        let resp = ui
            .interact(all, Id::unique("desktop-all"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Every conversation (Ctrl+F)");
        ui.painter().text(
            all.center(),
            Align2::CENTER_CENTER,
            "All",
            FontId::proportional(12.0),
            if resp.hovered() { t.ink } else { t.ink3 },
        );
        if resp.clicked() {
            self.home_menu = true;
            self.home_list = home::LOCAL_TAB.into();
        }
        if rows.is_empty() {
            crate::widgets::center_note(ui, t, body, "No conversations yet");
            return false;
        }
        let row_h = 44.0;
        let n = ((body.height() + 2.0) / row_h).floor().max(0.0) as usize;
        for (i, r) in rows.iter().take(n).enumerate() {
            let rect = Rect::from_min_size(
                egui::pos2(body.left() - 8.0, body.top() + i as f32 * row_h),
                vec2(body.width() + 16.0, row_h - 2.0),
            );
            if Self::chat_tile_row(
                ui,
                t,
                Tile::Recent,
                rect,
                r,
                &format!("{}  {}", r.folder, crate::time::ago(r.at)),
            ) {
                self.open(r.id.clone());
                return true;
            }
        }
        false
    }

    /// The conversations at work or paused; when there are none, the arch,
    /// faint, and a note.
    fn agents(&mut self, ui: &mut Ui, t: &Theme, body: Rect, rows: &[ChatRow]) {
        let working: Vec<&ChatRow> = rows.iter().filter(|r| r.busy || r.paused).collect();
        if working.is_empty() {
            let logo = Rect::from_center_size(body.center() - vec2(0.0, 14.0), vec2(56.0, 56.0));
            gravity::paint_logo(ui.painter(), logo, Some(t.ink.gamma_multiply(0.10)));
            ui.painter().text(
                egui::pos2(body.center().x, logo.bottom() + 8.0),
                Align2::CENTER_TOP,
                "Nothing running",
                FontId::proportional(12.5),
                t.ink3,
            );
            return;
        }
        let row_h = 44.0;
        let n = ((body.height() + 2.0) / row_h).floor().max(0.0) as usize;
        for (i, r) in working.iter().take(n).enumerate() {
            let rect = Rect::from_min_size(
                egui::pos2(body.left() - 8.0, body.top() + i as f32 * row_h),
                vec2(body.width() + 16.0, row_h - 2.0),
            );
            let what = if r.paused { "Paused" } else { "Working" };
            if Self::chat_tile_row(
                ui,
                t,
                Tile::Agents,
                rect,
                r,
                &format!("{what}, {}", r.folder),
            ) {
                self.open(r.id.clone());
                return;
            }
        }
    }

    /// A conversation's row in a tile: its dot when at work or paused, its
    /// title and `sub` under it; lit under the pointer. Whether clicked.
    fn chat_tile_row(
        ui: &mut Ui,
        t: &Theme,
        tile: Tile,
        rect: Rect,
        r: &ChatRow,
        sub: &str,
    ) -> bool {
        let resp = ui
            .interact(
                rect,
                Id::unique(("desktop-row", tile as u8, &r.id)),
                Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, 10.0, Color32::from_white_alpha(10));
        }
        let mut x = rect.left() + 10.0;
        if r.busy || r.paused {
            crate::widgets::state_dot(p, t, egui::pos2(x + 3.0, rect.top() + 14.0), r.paused);
            x += 14.0;
        }
        let w = rect.right() - 10.0 - x;
        let title = FontId::proportional(13.5);
        let small = FontId::proportional(11.5);
        p.text(
            egui::pos2(x, rect.top() + 14.0),
            Align2::LEFT_CENTER,
            crate::widgets::clip_width(ui, &r.title, &title, w),
            title,
            if resp.hovered() {
                t.ink
            } else {
                t.ink2.lerp_to_gamma(t.ink, 0.5)
            },
        );
        p.text(
            egui::pos2(x, rect.top() + 31.0),
            Align2::LEFT_CENTER,
            crate::widgets::clip_width(ui, sub, &small, w),
            small,
            t.ink3,
        );
        resp.clicked()
    }

    /// The dock at the bottom of `rect`: Home's menu on each of its lists,
    /// New conversation and Settings. Gives its rectangle.
    pub(crate) fn dock(&mut self, ui: &mut Ui, t: &Theme, rect: Rect) -> Rect {
        enum Act {
            List(&'static str),
            New,
            Settings,
        }
        let items: [(&str, &str, Act); 6] = [
            (
                "\u{f086}",
                "Conversations (Ctrl+F)",
                Act::List(home::LOCAL_TAB),
            ),
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
        p.rect_stroke(
            dock,
            20.0,
            Stroke::new(1.0, Color32::from_white_alpha(22)),
            egui::StrokeKind::Inside,
        );
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
            Some((Act::List(id), on)) => {
                self.home_menu = !on;
                self.home_list = id.into();
                if !on && id == home::LOCAL_TAB {
                    ui.memory_mut(|m| m.request_focus(Id::unique("home-chat-search")));
                }
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

/// A tile: a soft shadow, the panel a little see-through over the
/// wallpaper, a hairline border, light along its top edge, and its heading
/// (a dot in `mark`, the title spaced out).
fn paint_tile(p: &egui::Painter, t: &Theme, r: Rect, title: &str, mark: Color32) {
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
    p.rect_stroke(
        r,
        TILE_R,
        Stroke::new(1.0, Color32::from_white_alpha(18)),
        egui::StrokeKind::Inside,
    );
    p.hline(
        r.left() + TILE_R..=r.right() - TILE_R,
        r.top() + 1.0,
        Stroke::new(1.0, Color32::from_white_alpha(14)),
    );
    let y = r.top() + HEAD_H / 2.0 + 2.0;
    p.circle_filled(egui::pos2(r.left() + 22.0, y), 3.5, mark);
    p.circle_filled(
        egui::pos2(r.left() + 22.0, y),
        7.0,
        mark.gamma_multiply(0.15),
    );
    spaced(p, egui::pos2(r.left() + 34.0, y), title, 11.0, t.ink3);
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
fn calendar(
    p: &egui::Painter,
    t: &Theme,
    body: Rect,
    now: &chrono::DateTime<Local>,
    rows: &[ChatRow],
) {
    let first = now.date_naive().with_day(1).unwrap();
    let days = {
        let next = if now.month() == 12 {
            chrono::NaiveDate::from_ymd_opt(now.year() + 1, 1, 1)
        } else {
            chrono::NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1)
        };
        next.unwrap().signed_duration_since(first).num_days() as u32
    };
    let active: std::collections::HashSet<u32> = rows
        .iter()
        .filter_map(|r| chrono::DateTime::from_timestamp_millis(r.at))
        .map(|d| d.with_timezone(&Local))
        .filter(|d| d.year() == now.year() && d.month() == now.month())
        .map(|d| d.day())
        .collect();
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
    for day in 1..=days {
        let cell = lead + day - 1;
        let (col, row) = (cell % 7, cell / 7);
        let c = egui::pos2(
            grid.left() + cw * (col as f32 + 0.5),
            grid.top() + ch * (row as f32 + 1.5),
        );
        let today = day == now.day();
        if today {
            p.circle_filled(c, (ch * 0.42).min(cw * 0.42), t.accent);
        }
        let weekend = col == 0 || col == 6;
        let color = if today {
            Color32::WHITE
        } else if day < now.day() {
            t.ink3
        } else if weekend {
            t.ink3
        } else {
            t.ink2
        };
        p.text(
            c,
            Align2::CENTER_CENTER,
            day.to_string(),
            num.clone(),
            color,
        );
        if active.contains(&day) && !today {
            p.circle_filled(c + vec2(0.0, ch * 0.32), 1.8, hex(MARKS[1]));
        }
    }
}

/// Four numbers, two by two: the conversations, those at work, those
/// paused and the tabs open, each with its color's bar; Oak's version under
/// them.
fn overview(p: &egui::Painter, t: &Theme, body: Rect, rows: &[ChatRow], tabs: usize) {
    let busy = rows.iter().filter(|r| r.busy).count();
    let paused = rows.iter().filter(|r| r.paused).count();
    let cells = [
        (rows.len(), "Conversations"),
        (busy, "At work"),
        (paused, "Paused"),
        (tabs, "Tabs open"),
    ];
    let foot = 22.0;
    let grid = Rect::from_min_max(body.min, egui::pos2(body.right(), body.bottom() - foot));
    let (cw, ch) = ((grid.width() - 12.0) / 2.0, (grid.height() - 12.0) / 2.0);
    let big = FontId::new(
        (ch * 0.42).clamp(18.0, 34.0),
        egui::FontFamily::Name("bold".into()),
    );
    for (i, (n, label)) in cells.iter().enumerate() {
        let cell = Rect::from_min_size(
            grid.min + vec2((i % 2) as f32 * (cw + 12.0), (i / 2) as f32 * (ch + 12.0)),
            vec2(cw, ch),
        );
        let mark = hex(MARKS[i]);
        p.rect_filled(cell, 12.0, Color32::from_white_alpha(6));
        p.rect_filled(
            Rect::from_min_size(cell.min + vec2(12.0, 12.0), vec2(18.0, 3.0)),
            1.5,
            mark,
        );
        p.text(
            egui::pos2(cell.left() + 12.0, cell.center().y + 2.0),
            Align2::LEFT_CENTER,
            n.to_string(),
            big.clone(),
            t.ink,
        );
        p.text(
            egui::pos2(cell.left() + 12.0, cell.bottom() - 10.0),
            Align2::LEFT_BOTTOM,
            *label,
            FontId::proportional(11.5),
            t.ink3,
        );
    }
    p.text(
        egui::pos2(body.left(), body.bottom()),
        Align2::LEFT_BOTTOM,
        format!("Oak {}", crate::update::VERSION),
        FontId::proportional(11.0),
        t.ink4,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each width gets its grid, and no tile overlaps another or leaves
    /// the area.
    #[test]
    fn tiles_fit_and_never_overlap() {
        for (w, h) in [(1400.0, 800.0), (800.0, 790.0), (500.0, grid_height(500.0))] {
            let area = Rect::from_min_size(egui::pos2(10.0, 10.0), vec2(w, h));
            let tiles = layout(area);
            assert_eq!(tiles.len(), 6, "{w}x{h}");
            for (i, (_, a)) in tiles.iter().enumerate() {
                assert!(area.expand(0.5).contains_rect(*a), "{w}x{h}: {a:?}");
                for (_, b) in &tiles[i + 1..] {
                    assert!(!a.shrink(0.5).intersects(*b), "{w}x{h}: {a:?} {b:?}");
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
}
