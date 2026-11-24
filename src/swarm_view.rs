//! The Swarm view (/swarm): a swarm's coordinator conversation gets tabs
//! above its transcript. Chat is the transcript; Board shows every post,
//! live, mentions colored; Members lists each member's state, model, who
//! added it and how much it posted (a click opens its conversation, the
//! right-click menu removes it); Graph draws the members around the
//! coordinator, with an arrow for who named whom on the board.

use crate::agent::swarm::{MemberState, state_of};
use crate::swarm::{self, COORDINATOR, MONITOR, Member, Swarm};
use crate::theme::{Theme, mix};
use crate::time::{clock12, full_time, local_offset_ms};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, Ui, UiBuilder, text::LayoutJob, vec2};

/// The tab strip's height.
pub(crate) const STRIP_H: f32 = 34.0;

/// What the coordinator's tab shows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Tab {
    #[default]
    Chat,
    Board,
    Members,
    Graph,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Chat, Tab::Board, Tab::Members, Tab::Graph];

    fn label(self) -> &'static str {
        match self {
            Tab::Chat => "Chat",
            Tab::Board => "Board",
            Tab::Members => "Members",
            Tab::Graph => "Graph",
        }
    }
}

/// What the user asked for in the view.
pub(crate) enum Action {
    /// Open this conversation (a member's).
    Open(String),
    /// Remove this member from the swarm, for good.
    Kill(String),
}

/// A member as the view shows it.
struct Row {
    m: Member,
    state: MemberState,
    model: String,
    posts: usize,
}

fn rows(s: &Swarm) -> Vec<Row> {
    s.roster
        .members
        .iter()
        .map(|m| {
            let (state, model) = state_of(m);
            Row { posts: s.posts.iter().filter(|p| p.from == m.name).count(), m: m.clone(), state, model }
        })
        .collect()
}

/// A state's color: working the accent, waiting faint, stopped red, killed
/// fainter still.
fn state_color(t: &Theme, s: &MemberState) -> Color32 {
    match s {
        MemberState::Working => t.accent,
        MemberState::Waiting => t.ink3,
        MemberState::Paused | MemberState::Failed(_) | MemberState::Gone => t.error,
        MemberState::Killed => t.line,
    }
}

/// A name's color on the board: the coordinator's is the accent, the
/// monitor's faint, every other one of a few hues, by its name.
fn name_color(t: &Theme, name: &str) -> Color32 {
    const HUES: [Color32; 6] = [
        Color32::from_rgb(0x4e, 0xa8, 0xde),
        Color32::from_rgb(0x3f, 0xb9, 0x50),
        Color32::from_rgb(0xd2, 0x8b, 0x26),
        Color32::from_rgb(0xb0, 0x7c, 0xe8),
        Color32::from_rgb(0xe0, 0x6c, 0x75),
        Color32::from_rgb(0x2a, 0xb3, 0xa6),
    ];
    match name {
        COORDINATOR => t.accent,
        MONITOR => t.ink3,
        _ => HUES[name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % HUES.len()],
    }
}

/// The tab strip in `rect`: the tabs (a click picks one), then the
/// swarm's title and how many members work.
pub(crate) fn strip(ui: &mut Ui, t: &Theme, rect: Rect, tab: &mut Tab, swarm_id: &str) {
    let sw = swarm::get(swarm_id);
    let (title, members) = {
        let s = sw.lock().unwrap();
        (s.roster.title.clone(), s.roster.members.clone())
    };
    let live: Vec<&Member> = members.iter().filter(|m| !m.killed && m.name != COORDINATOR).collect();
    let working = live.iter().filter(|m| state_of(m).0 == MemberState::Working).count();
    ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for tb in Tab::ALL {
                let on = *tab == tb;
                let text = RichText::new(tb.label()).color(if on { t.ink } else { t.ink3 });
                if ui.selectable_label(on, text).clicked() {
                    *tab = tb;
                }
            }
            ui.add_space(10.0);
            let title = if title.is_empty() { "Swarm".to_string() } else { title };
            let count = format!("{working} of {} working", live.len());
            let label = ui.add(egui::Label::new(RichText::new(format!("{title}, {count}")).small().color(t.ink3)).truncate());
            label.on_hover_text("The swarm this conversation coordinates (/swarm)");
        });
    });
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, t.line));
}

/// Tab `tab` (not Chat) of swarm `swarm_id`, in the transcript's column
/// `col` of the ui's rect.
pub(crate) fn show(ui: &mut Ui, t: &Theme, col: egui::Rangef, swarm_id: &str, tab: Tab) -> Option<Action> {
    let sw = swarm::get(swarm_id);
    let s = sw.lock().unwrap();
    // (Kept current: members' states change without a post.)
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    let area = ui.max_rect();
    let rect = Rect::from_x_y_ranges(col, area.top() + 8.0..=area.bottom());
    let mut action = None;
    ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| match tab {
        Tab::Board => board(ui, t, &s),
        Tab::Members => action = members(ui, t, &s),
        Tab::Graph => action = graph(ui, t, &s),
        Tab::Chat => {}
    });
    action
}

/// Every post, oldest first, kept scrolled to the newest.
fn board(ui: &mut Ui, t: &Theme, s: &Swarm) {
    if s.posts.is_empty() {
        ui.label(RichText::new("Nothing on the board yet").color(t.ink3));
        return;
    }
    let off = local_offset_ms();
    ScrollArea::vertical().id_salt(("swarm-board", &s.id)).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for p in &s.posts {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new(format!("#{}", p.seq)).small().color(t.ink3));
                ui.label(RichText::new(format!("@{}", p.from)).strong().color(name_color(t, &p.from)));
                ui.label(RichText::new(clock12(p.at, off)).small().color(t.ink3)).on_hover_text(full_time(p.at, off));
            });
            let mut job = LayoutJob::default();
            let font = egui::TextStyle::Body.resolve(ui.style());
            for (run, mention) in swarm::split_mentions(&p.text) {
                let color = if mention { name_color(t, &run[1..].to_lowercase()) } else { t.ink };
                job.append(run, 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
            }
            job.wrap.max_width = ui.available_width();
            ui.add(egui::Label::new(job).wrap().selectable(true));
            ui.add_space(10.0);
        }
    });
}

/// The members: state, name, model, who added it, posts.
fn members(ui: &mut Ui, t: &Theme, s: &Swarm) -> Option<Action> {
    let mut action = None;
    ScrollArea::vertical().id_salt(("swarm-members", &s.id)).auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new(("swarm-members-grid", &s.id)).num_columns(5).spacing(vec2(18.0, 8.0)).show(ui, |ui| {
            for h in ["Member", "State", "Model", "Added by", "Posts"] {
                ui.label(RichText::new(h).small().color(t.ink3));
            }
            ui.end_row();
            for r in rows(s) {
                let color = state_color(t, &r.state);
                let name = ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                    let text = RichText::new(format!("@{}", r.m.name)).color(if r.m.killed { t.ink3 } else { t.ink });
                    ui.add(egui::Label::new(text).sense(Sense::click()))
                });
                let resp = name.inner.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open its conversation");
                if resp.clicked() {
                    action = Some(Action::Open(r.m.conv.clone()));
                }
                if r.m.name != COORDINATOR && !r.m.killed {
                    resp.context_menu(|ui| {
                        if ui.button("Remove from the swarm").on_hover_text("For good: it stops, and its name can't be used again").clicked() {
                            action = Some(Action::Kill(r.m.name.clone()));
                            ui.close();
                        }
                    });
                }
                ui.label(RichText::new(r.state.describe()).color(color));
                ui.label(RichText::new(if r.model.is_empty() { "?".into() } else { crate::api::model_name(&r.model) }).color(t.ink2));
                let by = if r.m.added_by.is_empty() { String::new() } else { format!("@{}", r.m.added_by) };
                ui.label(RichText::new(by).color(t.ink3));
                ui.label(RichText::new(r.posts.to_string()).color(t.ink3));
                ui.end_row();
            }
        });
    });
    action
}

/// Where the graph's nodes go in `rect`: the first (the coordinator) in
/// its middle, the others evenly around it.
fn layout(n: usize, rect: Rect) -> Vec<Pos2> {
    let c = rect.center();
    let r = (rect.width().min(rect.height()) * 0.36).max(40.0);
    (0..n)
        .map(|i| {
            if i == 0 {
                return c;
            }
            let a = std::f32::consts::TAU * (i - 1) as f32 / (n - 1) as f32 - std::f32::consts::FRAC_PI_2;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// The members as nodes (colored by state), with an arrow for each pair
/// where one named the other on the board (thicker for more posts).
fn graph(ui: &mut Ui, t: &Theme, s: &Swarm) -> Option<Action> {
    let rows = rows(s);
    if rows.len() < 2 {
        ui.label(RichText::new("No members yet").color(t.ink3));
        return None;
    }
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let at = layout(rows.len(), rect.shrink(30.0));
    let p = ui.painter_at(rect);
    let node = 15.0;
    let pos_of = |name: &str| rows.iter().position(|r| r.m.name == name).map(|i| at[i]);
    for (from, to, n) in s.edges() {
        let (Some(a), Some(b)) = (pos_of(&from), pos_of(&to)) else { continue };
        let dir = (b - a).normalized();
        // (Each way of a pair a little apart, so both show.)
        let side = vec2(-dir.y, dir.x) * 3.0;
        let (a, b) = (a + dir * node + side, b - dir * (node + 2.0) + side);
        let stroke = Stroke::new(1.0 + (n.min(8) as f32) * 0.4, mix(t.bg, t.ink3, 0.8));
        p.line_segment([a, b], stroke);
        let head = [b, b - dir * 8.0 + vec2(-dir.y, dir.x) * 4.0, b - dir * 8.0 - vec2(-dir.y, dir.x) * 4.0];
        p.add(egui::Shape::convex_polygon(head.to_vec(), stroke.color, Stroke::NONE));
    }
    let mut action = None;
    let font = FontId::proportional(12.0);
    for (i, (r, &c)) in rows.iter().zip(&at).enumerate() {
        let color = state_color(t, &r.state);
        let id = egui::Id::unique(("swarm-node", &s.id, &r.m.name));
        let resp = ui.interact(Rect::from_center_size(c, vec2(node * 2.0, node * 2.0)), id, Sense::click());
        let fill = if resp.hovered() { mix(t.panel, color, 0.25) } else { t.panel };
        p.circle(c, node, fill, Stroke::new(2.0, color));
        // Outer nodes put their names outside the ring, so arrows into them
        // never cross their text.
        let above = i > 0 && c.y < at[0].y - 10.0;
        let (pos, align) = if above {
            (c - vec2(0.0, node + 4.0), Align2::CENTER_BOTTOM)
        } else {
            (c + vec2(0.0, node + 4.0), Align2::CENTER_TOP)
        };
        p.text(pos, align, format!("@{}", r.m.name), font.clone(), if r.m.killed { t.ink3 } else { t.ink });
        let tip = format!("@{}: {}{}", r.m.name, r.state.describe(), if r.model.is_empty() { String::new() } else { format!("\n{}", crate::api::model_name(&r.model)) });
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
            action = Some(Action::Open(r.m.conv.clone()));
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{click, painted, rect_of, run_frame, test_ctx};

    /// The strip's tabs and line, and the Board, Members and Graph tabs,
    /// as painted; a click on a tab picks it, on a member opens it.
    #[test]
    fn the_swarm_view_shows_the_board_members_and_graph() {
        let id = format!("swarm-view-{}", std::process::id());
        {
            let sw = swarm::get(&id);
            let mut s = sw.lock().unwrap();
            s.roster.title = "Parser rewrite".into();
            s.roster.members = vec![
                Member { name: COORDINATOR.into(), conv: id.clone(), ..Default::default() },
                Member { name: "parser".into(), conv: format!("{id}-agent1"), added_by: COORDINATOR.into(), ..Default::default() },
                Member { name: "old".into(), conv: format!("{id}-agent2"), added_by: COORDINATOR.into(), killed: true, ..Default::default() },
            ];
            s.save();
            s.post(COORDINATOR, "@parser take src/parse.rs", 1);
        }
        let (ctx, t) = test_ctx(true, true);
        let size = vec2(1000.0, 700.0);
        let mut time = 0.0;
        let mut tab = Tab::Chat;
        let mut frame = |tab: &mut Tab, events| {
            let mut action = None;
            let out = run_frame(&ctx, size, &mut time, 0.05, events, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let r = ui.max_rect();
                    strip(ui, &t, Rect::from_min_size(r.min, vec2(r.width(), STRIP_H)), tab, &id);
                    let rest = Rect::from_min_max(r.min + vec2(0.0, STRIP_H), r.max);
                    if *tab != Tab::Chat {
                        action = ui.scope_builder(UiBuilder::new().max_rect(rest), |ui| show(ui, &t, rest.x_range(), &id, *tab)).inner;
                    }
                });
            });
            (painted(&out), action)
        };
        let (shapes, _) = frame(&mut tab, vec![]);
        // (Killed members don't count.)
        rect_of(&shapes, "Parser rewrite, 0 of 1 working");
        let at = |shapes: &[(String, Rect)], s: &str| click(rect_of(shapes, s).center(), egui::PointerButton::Primary);
        frame(&mut tab, at(&shapes, "Board"));
        assert_eq!(tab, Tab::Board);
        let (shapes, _) = frame(&mut tab, vec![]);
        rect_of(&shapes, "@coordinator");
        assert!(shapes.iter().any(|(s, _)| s.contains("take src/parse.rs")), "{shapes:?}");
        frame(&mut tab, at(&shapes, "Members"));
        let (shapes, _) = frame(&mut tab, vec![]);
        rect_of(&shapes, "Added by");
        rect_of(&shapes, "killed");
        let (_, action) = frame(&mut tab, at(&shapes, "@parser"));
        assert!(matches!(action, Some(Action::Open(c)) if c == format!("{id}-agent1")));
        frame(&mut tab, at(&shapes, "Graph"));
        let (shapes, _) = frame(&mut tab, vec![]);
        rect_of(&shapes, "@old");
        swarm::remove(&id);
    }

    #[test]
    fn the_coordinator_sits_in_the_middle_of_the_graph() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0));
        let at = layout(5, rect);
        assert_eq!(at[0], rect.center());
        let r: Vec<f32> = at[1..].iter().map(|p| (*p - rect.center()).length()).collect();
        assert!(r.iter().all(|d| (d - r[0]).abs() < 0.01 && *d > 50.0));
        // The first member straight above it.
        assert!((at[1].x - rect.center().x).abs() < 0.01 && at[1].y < rect.center().y);
    }
}
