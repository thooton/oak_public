//! The Swarm view (/swarm): a swarm's coordinator conversation gets tabs
//! above its transcript. Chat is the transcript; Board shows every post,
//! live, mentions colored; Members lists each member's state, model, who
//! added it and how much it posted (a click opens its conversation, the
//! right-click menu removes it); Graph is the swarm alive: the members as
//! Oak bots (`bot.rs`) settling around the coordinator by a little force
//! simulation, each post a pulse along the edge from its author to those it
//! names (see `graph`).

use crate::agent::swarm::{MemberState, state_of};
use crate::bot;
use crate::swarm::{self, COORDINATOR, MONITOR, Member, Swarm};
use crate::theme::{Theme, mix};
use crate::time::{clock12, full_time, local_offset_ms};
use eframe::egui::{self, Align2, Color32, FontId, Mesh, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, Ui, UiBuilder, Vec2, text::LayoutJob, vec2};
use std::collections::HashMap;
use std::f32::consts::TAU;

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
pub(crate) fn name_color(t: &Theme, name: &str) -> Color32 {
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
        _ => HUES[hash(name) % HUES.len()],
    }
}

/// A name's hash, for its hue and its animations' phase.
fn hash(name: &str) -> usize {
    name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize))
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

/// Where the graph's nodes start in `rect`: the first (the coordinator) in
/// its middle, the others evenly around it.
fn layout(n: usize, rect: Rect) -> Vec<Pos2> {
    let c = rect.center();
    let r = (rect.width().min(rect.height()) * 0.36).max(40.0);
    (0..n)
        .map(|i| {
            if i == 0 {
                return c;
            }
            let a = TAU * (i - 1) as f32 / (n - 1) as f32 - std::f32::consts::FRAC_PI_2;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// How long a post's pulse takes along its edge, and the ripple it sets
/// off at either end (seconds).
const TRAVEL: f64 = 0.9;
const RIPPLE: f64 = 0.7;
/// How long a new member takes to pop in (seconds).
const SPAWN: f64 = 0.6;
/// The simulation's step (seconds), and how much speed a step keeps.
const DT: f32 = 1.0 / 60.0;
const DAMP: f32 = 0.86;

/// A post on its way from its author to one it names.
#[derive(Clone)]
struct Pulse {
    from: String,
    to: String,
    /// When it leaves (ui time; a post naming several leaves a little
    /// after each other).
    t0: f64,
}

/// Who named whom: how often, and when last (Unix ms).
#[derive(Clone)]
struct Edge {
    from: String,
    to: String,
    n: u32,
    last_at: i64,
}

/// What the graph keeps between frames (egui's memory, per swarm): where
/// each node is and how fast it moves, the view's pan and zoom, the pulses
/// under way, and what it read off the board at `version`.
#[derive(Clone, Default)]
struct GraphState {
    pos: HashMap<String, Vec2>,
    vel: HashMap<String, Vec2>,
    /// When a member that joined while the graph was shown appeared.
    born: HashMap<String, f64>,
    pan: Vec2,
    /// 0 until set (then 1).
    zoom: f32,
    /// The member being dragged.
    drag: Option<String>,
    /// The board's head when last drawn (None before the first frame: what
    /// was posted before then sends no pulses).
    seen: Option<u64>,
    pulses: Vec<Pulse>,
    version: Option<u64>,
    edges: Vec<Edge>,
    /// Each member's last post.
    last: HashMap<String, String>,
    time: f64,
}

impl GraphState {
    /// What it reads off the board, again if the board changed.
    fn read(&mut self, s: &Swarm) {
        if self.version == Some(s.version) {
            return;
        }
        self.version = Some(s.version);
        let mut last_at: HashMap<(&str, &str), i64> = HashMap::new();
        for p in &s.posts {
            for m in s.roster.members.iter().filter(|m| m.name != p.from && Swarm::names_member(p, &m.name)) {
                last_at.insert((p.from.as_str(), m.name.as_str()), p.at);
            }
            self.last.insert(p.from.clone(), p.text.clone());
        }
        self.edges = s.edges().into_iter().map(|(from, to, n)| Edge { last_at: last_at.get(&(from.as_str(), to.as_str())).copied().unwrap_or(0), from, to, n }).collect();
    }

    /// A pulse for each member each new post names: posts of the last few
    /// seconds only, so coming back to the tab doesn't replay what came
    /// while it was hidden (`wall`: now, Unix ms).
    fn pulses(&mut self, s: &Swarm, now: f64, wall: i64) {
        if let Some(seen) = self.seen {
            for p in s.posts.iter().filter(|p| p.seq > seen && p.at >= wall - 5_000) {
                let to = s.roster.members.iter().filter(|m| m.name != p.from && Swarm::names_member(p, &m.name));
                for (k, m) in to.enumerate() {
                    self.pulses.push(Pulse { from: p.from.clone(), to: m.name.clone(), t0: now + k as f64 * 0.12 });
                }
            }
        }
        self.seen = Some(s.head());
        self.pulses.retain(|p| now < p.t0 + TRAVEL + RIPPLE);
        // (A board that floods doesn't flood the frame.)
        let extra = self.pulses.len().saturating_sub(64);
        self.pulses.drain(..extra);
    }

    /// `steps` steps of the layout's forces on `names` (the coordinator
    /// first, held still, as is the member being dragged): each member
    /// springs to `l` from the coordinator, the members push each other
    /// apart, and those that talk pull together a little. The fastest
    /// node's speed after.
    fn simulate(&mut self, names: &[&str], links: &[(usize, usize, u32)], l: f32, steps: usize) -> f32 {
        let n = names.len();
        let mut p: Vec<Vec2> = names.iter().map(|k| self.pos.get(*k).copied().unwrap_or_default()).collect();
        let mut v: Vec<Vec2> = names.iter().map(|k| self.vel.get(*k).copied().unwrap_or_default()).collect();
        let held: Vec<bool> = names.iter().enumerate().map(|(i, k)| i == 0 || self.drag.as_deref() == Some(*k)).collect();
        let ks = 30.0;
        let kr = ks * 0.15 * l * l * l;
        let mut fastest: f32 = 0.0;
        for _ in 0..steps {
            let mut f = vec![Vec2::ZERO; n];
            for i in 0..n {
                for j in i + 1..n {
                    let d = p[i] - p[j];
                    let d2 = d.length_sq().max(25.0);
                    let push = if d.length_sq() < 1e-6 { vec2(1.0, 0.0) } else { d.normalized() } * (kr / d2);
                    f[i] += push;
                    f[j] -= push;
                }
            }
            for i in 1..n {
                let d = p[i] - p[0];
                let len = d.length().max(1.0);
                f[i] -= d / len * ks * (len - l);
            }
            for &(a, b, c) in links {
                if a == 0 || b == 0 {
                    continue;
                }
                let d = p[b] - p[a];
                let len = d.length().max(1.0);
                let pull = d / len * ks * 0.25 * (c.min(6) as f32 / 6.0) * (len - 0.8 * l);
                f[a] += pull;
                f[b] -= pull;
            }
            fastest = 0.0;
            for i in 0..n {
                if held[i] {
                    v[i] = Vec2::ZERO;
                    continue;
                }
                v[i] = (v[i] + f[i] * DT) * DAMP;
                if v[i].length() > 3000.0 {
                    v[i] = v[i].normalized() * 3000.0;
                }
                p[i] += v[i] * DT;
                fastest = fastest.max(v[i].length());
            }
        }
        for (i, k) in names.iter().enumerate() {
            self.pos.insert(k.to_string(), p[i]);
            self.vel.insert(k.to_string(), v[i]);
        }
        fastest
    }
}

/// A soft round glow: `color` (premultiplied) at `c`, fading out by `r`.
fn glow(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let n = 40u32;
    let mut m = Mesh::default();
    m.colored_vertex(c, color);
    for (ring, (f, a)) in [(0.45, 0.4), (1.0, 0.0)].into_iter().enumerate() {
        for i in 0..n {
            let ang = TAU * i as f32 / n as f32;
            m.colored_vertex(c + vec2(ang.cos(), ang.sin()) * r * f, color.gamma_multiply(a));
        }
        let base = 1 + ring as u32 * n;
        for i in 0..n {
            let (j, k) = (base + i, base + (i + 1) % n);
            if ring == 0 {
                m.add_triangle(0, j, k);
            } else {
                m.add_triangle(j - n, k - n, j);
                m.add_triangle(k - n, j, k);
            }
        }
    }
    p.add(m);
}

/// The quadratic curve from a node at `a` (radius `ra`) to one at `b`
/// (radius `rb`), bowed to its left so the two ways of a pair part: its
/// start, control and end points, or none for nodes too close.
fn curve(a: Pos2, b: Pos2, ra: f32, rb: f32) -> Option<[Pos2; 3]> {
    let d = b - a;
    let len = d.length();
    if len < ra + rb + 6.0 {
        return None;
    }
    let bend = vec2(-d.y, d.x) / len * len * 0.12;
    let mid = a + d * 0.5 + bend;
    let s = a + (mid - a).normalized() * ra;
    let e = b + (mid - b).normalized() * rb;
    Some([s, s + (e - s) * 0.5 + bend, e])
}

fn bezier(q: &[Pos2; 3], t: f32) -> Pos2 {
    let u = 1.0 - t;
    (q[0].to_vec2() * u * u + q[1].to_vec2() * 2.0 * u * t + q[2].to_vec2() * t * t).to_pos2()
}

fn smooth(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// A pop that overshoots a little and settles, for 0..=1.
fn pop(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0) - 1.0;
    1.0 + 2.7 * x * x * x + 1.7 * x * x
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The swarm, alive: the members as Oak bots in their colors (the
/// coordinator in the middle, bigger), laid out by a little force
/// simulation (each member springs to a ring around the coordinator, the
/// members push apart, those that talk pull together). A working bot
/// breathes in a glow and bobs; a waiting one dims; a stopped one dims in a
/// red ring; a killed one is faded grey with a small x. (No eyes, as the
/// running-agents list's bots.) An
/// edge for each pair where one named the other, thicker for more posts and
/// lit in its author's color for a while after the last; a new post sends a
/// pulse along it, with a ripple where it leaves and lands. Hovering a bot
/// says its role, state, model, who added it, its posts and its last one,
/// and lights its edges; a click opens its conversation; a drag moves it
/// (it springs back); dragging the background pans, scrolling zooms and a
/// double click resets the view. It repaints at full rate only while
/// something moves, at 30 fps while a member works, else not at all.
fn graph(ui: &mut Ui, t: &Theme, s: &Swarm) -> Option<Action> {
    let rows = rows(s);
    if rows.len() < 2 {
        ui.label(RichText::new("No members yet").color(t.ink3));
        return None;
    }
    let (rect, bg) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let id = egui::Id::unique(("swarm-graph", &s.id));
    let mut g: GraphState = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    let wall = now_ms();
    if g.zoom == 0.0 {
        g.zoom = 1.0;
    }
    let l = (rect.width().min(rect.height()) * 0.29).clamp(80.0, 260.0);
    g.read(s);
    g.pulses(s, now, wall);

    // New members: at their place on the ring (a little off, so none sit
    // exactly on another), popping in unless they were there already.
    let names: Vec<&str> = rows.iter().map(|r| r.m.name.as_str()).collect();
    let first = g.pos.is_empty();
    let seeds = layout(rows.len(), Rect::from_center_size(Pos2::ZERO, Vec2::splat(l / 0.36)));
    for (i, name) in names.iter().enumerate() {
        if !g.pos.contains_key(*name) {
            let h = hash(name);
            let jitter = if i == 0 { Vec2::ZERO } else { vec2((h % 7) as f32 - 3.0, (h / 7 % 7) as f32 - 3.0) };
            g.pos.insert(name.to_string(), seeds[i].to_vec2() + jitter);
            if !first {
                g.born.insert(name.to_string(), now);
            }
        }
    }
    g.pos.insert(names[0].to_string(), Vec2::ZERO);
    g.born.retain(|_, b| now - *b < SPAWN);

    // Zoom about the pointer (wheel or pinch), pan by dragging the
    // background, a double click puts the view back.
    if ui.rect_contains_pointer(rect) {
        let (zd, sc, ptr) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.y, i.pointer.hover_pos()));
        let f = zd * (sc * 0.002).exp();
        if let Some(ptr) = ptr.filter(|_| (f - 1.0).abs() > 1e-4) {
            let z = (g.zoom * f).clamp(0.35, 3.0);
            let world = (ptr - rect.center() - g.pan) / g.zoom;
            g.pan = ptr - rect.center() - world * z;
            g.zoom = z;
        }
    }
    if bg.dragged() {
        g.pan += bg.drag_delta();
    }
    if bg.double_clicked() {
        g.pan = Vec2::ZERO;
        g.zoom = 1.0;
    }
    let z = g.zoom;
    let size_of = |i: usize, r: &Row| {
        let base = (if i == 0 { 54.0 } else { 40.0 }) * (if r.m.killed { 0.85 } else { 1.0 });
        (base * z).clamp(16.0, 96.0)
    };

    // The bots: hovered, clicked, dragged.
    let mut action = None;
    let mut hovered = None;
    for (i, r) in rows.iter().enumerate() {
        let c = rect.center() + g.pan + g.pos[&r.m.name] * z;
        let size = size_of(i, r);
        let resp = ui.interact(Rect::from_center_size(c, Vec2::splat(size * 0.9)), id.with(&r.m.name), Sense::click_and_drag());
        if resp.drag_started() && i > 0 {
            g.drag = Some(r.m.name.clone());
        }
        if resp.dragged() {
            if i == 0 {
                g.pan += resp.drag_delta();
            } else if let Some(p) = g.pos.get_mut(&r.m.name) {
                *p += resp.drag_delta() / z;
            }
        }
        if resp.drag_stopped() {
            g.drag = None;
        }
        if resp.hovered() || g.drag.as_deref() == Some(r.m.name.as_str()) {
            hovered = Some(i);
        }
        let mut resp = resp.on_hover_cursor(if g.drag.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::PointingHand });
        if g.drag.is_none() {
            let last = g.last.get(&r.m.name).map(|p| {
                let line = p.split_whitespace().collect::<Vec<_>>().join(" ");
                if line.chars().count() > 140 { format!("{}…", line.chars().take(140).collect::<String>()) } else { line }
            });
            resp = resp.on_hover_ui(|ui| {
                ui.set_max_width(320.0);
                ui.label(RichText::new(format!("@{}", r.m.name)).strong().color(name_color(t, &r.m.name)));
                ui.label(RichText::new(r.state.describe()).color(state_color(t, &r.state)));
                if !r.model.is_empty() {
                    ui.label(RichText::new(crate::api::model_name(&r.model)).color(t.ink2));
                }
                let by = if r.m.added_by.is_empty() { String::new() } else { format!("added by @{}, ", r.m.added_by) };
                ui.label(RichText::new(format!("{by}{} post{}", r.posts, crate::plural(r.posts as u64))).small().color(t.ink3));
                if let Some(last) = &last {
                    ui.add(egui::Label::new(RichText::new(last).small().color(t.ink2)).wrap());
                }
            });
        }
        if resp.clicked() {
            action = Some(Action::Open(r.m.conv.clone()));
        }
    }

    // The layout moves on (a few steps if frames came slow).
    let steps = (((now - g.time) * 60.0).round() as usize).clamp(1, 4);
    g.time = now;
    let index = |name: &str| names.iter().position(|n| *n == name);
    let links: Vec<(usize, usize, u32)> = g.edges.iter().filter_map(|e| Some((index(&e.from)?, index(&e.to)?, e.n))).collect();
    let fastest = g.simulate(&names, &links, l, steps);

    let p = ui.painter_at(rect);
    let origin = rect.center() + g.pan;
    let at = |name: &str| g.pos.get(name).map(|v| origin + *v * z);
    let radius = |name: &str| index(name).map_or(12.0, |i| size_of(i, &rows[i]) * 0.5 + 3.0);

    // The ground: dots that pan and zoom with the view, brighter near the
    // coordinator, its glow, and the ring the members settle on.
    let mut sp = 28.0 * z;
    while sp < 14.0 {
        sp *= 2.0;
    }
    while sp > 56.0 {
        sp /= 2.0;
    }
    let mut dots = Mesh::default();
    let mut y = rect.top() + (origin.y - rect.top()).rem_euclid(sp);
    while y < rect.bottom() {
        let mut x = rect.left() + (origin.x - rect.left()).rem_euclid(sp);
        while x < rect.right() {
            let near = (-(egui::pos2(x, y) - origin).length() / (l * z * 1.6)).exp();
            dots.add_colored_rect(Rect::from_center_size(egui::pos2(x, y), Vec2::splat(1.6)), mix(t.bg, t.ink, 0.05 + 0.10 * near));
            x += sp;
        }
        y += sp;
    }
    p.add(dots);
    glow(&p, origin, l * z * 1.1, t.accent.gamma_multiply(0.07));
    p.circle_stroke(origin, l * z, Stroke::new(1.0, mix(t.bg, t.line, 0.6)));

    // Edges, lit by how recently they carried a post; a hovered bot's stand
    // out and the others fade.
    let incident = |e: &Edge| hovered.is_some_and(|h| names[h] == e.from || names[h] == e.to);
    for e in &g.edges {
        let (Some(a), Some(b)) = (at(&e.from), at(&e.to)) else { continue };
        let Some(q) = curve(a, b, radius(&e.from), radius(&e.to) + 2.0) else { continue };
        let heat = (-((wall - e.last_at).max(0) as f32) / 30_000.0).exp();
        let hue = name_color(t, &e.from);
        let mut color = mix(mix(t.bg, t.ink3, 0.55), hue, 0.2 + 0.6 * heat);
        if hovered.is_some() {
            color = if incident(e) { mix(color, hue, 0.6) } else { mix(t.bg, color, 0.3) };
        }
        let width = (1.0 + e.n.min(10) as f32 * 0.3 + heat * 1.2) * z.sqrt().clamp(0.7, 1.4);
        let pts: Vec<Pos2> = (0..=24).map(|k| bezier(&q, k as f32 / 24.0)).collect();
        p.add(egui::Shape::line(pts, Stroke::new(width, color)));
        let dir = (q[2] - q[1]).normalized();
        let side = vec2(-dir.y, dir.x) * (3.0 + width);
        let back = q[2] - dir * (6.0 + width * 2.0);
        p.add(egui::Shape::convex_polygon(vec![q[2], back + side, back - side], color, Stroke::NONE));
    }

    // Pulses under way (a bright head with a fading trail), and the ripples
    // where they leave and land; a bot a pulse lands on bumps.
    let mut bump: HashMap<&str, f32> = HashMap::new();
    for pl in &g.pulses {
        let hue = name_color(t, &pl.from);
        let (Some(a), Some(b)) = (at(&pl.from), at(&pl.to)) else { continue };
        let age = now - pl.t0;
        if age < 0.0 {
            continue;
        }
        let ripple = |c: Pos2, r0: f32, age: f64| {
            if (0.0..RIPPLE).contains(&age) {
                let k = (age / RIPPLE) as f32;
                p.circle_stroke(c, r0 + k * 24.0 * z, Stroke::new(2.0 * (1.0 - k), hue.gamma_multiply(1.0 - k)));
            }
        };
        ripple(a, radius(&pl.from), age);
        ripple(b, radius(&pl.to), age - TRAVEL);
        let land = age - TRAVEL;
        if (0.0..RIPPLE).contains(&land) {
            let k = 1.0 - (land / RIPPLE) as f32;
            let e = bump.entry(pl.to.as_str()).or_default();
            *e = e.max(k);
        }
        if age < TRAVEL {
            let Some(q) = curve(a, b, radius(&pl.from), radius(&pl.to) + 2.0) else { continue };
            let u = smooth((age / TRAVEL) as f32);
            let head = bezier(&q, u);
            glow(&p, head, 12.0 * z.sqrt(), hue.gamma_multiply(0.55));
            for k in (1..8).rev() {
                let tu = u - k as f32 * 0.03;
                if tu > 0.0 {
                    let f = 1.0 - k as f32 / 8.0;
                    p.circle_filled(bezier(&q, tu), 2.8 * f * z.sqrt(), hue.gamma_multiply(0.7 * f));
                }
            }
            p.circle_filled(head, 3.4 * z.sqrt(), mix(hue, Color32::WHITE, 0.35));
        }
    }

    // The bots, with their names (and states) under them.
    let name_font = FontId::proportional((12.0 * z.sqrt()).clamp(10.0, 16.0));
    let state_font = FontId::proportional((10.5 * z.sqrt()).clamp(9.0, 13.0));
    let mut moving = false;
    for (i, r) in rows.iter().enumerate() {
        let Some(c0) = at(&r.m.name) else { continue };
        let phase = (hash(&r.m.name) % 997) as f32 / 997.0 * TAU;
        let hue = name_color(t, &r.m.name);
        let working = r.state == MemberState::Working && !r.m.killed;
        let breath = 0.5 + 0.5 * (now as f32 * 2.4 + phase).sin();
        let spawn = g.born.get(&r.m.name).map_or(1.0, |b| pop(((now - b) / SPAWN) as f32));
        moving |= spawn < 1.0;
        let scale = spawn * (1.0 + 0.14 * bump.get(r.m.name.as_str()).copied().unwrap_or(0.0));
        let size = size_of(i, r) * scale;
        let c = if working { c0 + vec2(0.0, (now as f32 * 1.6 + phase).sin() * 1.5 * z) } else { c0 };
        // (A working bot also swells a little with its breath.)
        let size = if working { size * (1.0 + 0.04 * breath) } else { size };
        let body = match &r.state {
            _ if r.m.killed => mix(t.bg, t.ink3, 0.35),
            MemberState::Working => hue,
            MemberState::Waiting => mix(t.bg, hue, 0.5),
            _ => mix(t.bg, hue, 0.4),
        };
        if working {
            glow(&p, c, size * (0.95 + 0.25 * breath), hue.gamma_multiply(0.18 + 0.17 * breath));
        }
        if i == 0 {
            p.circle_stroke(c, size * 0.68, Stroke::new(1.0, mix(t.bg, hue, 0.35)));
        }
        if matches!(r.state, MemberState::Paused | MemberState::Failed(_) | MemberState::Gone) && !r.m.killed {
            p.circle_stroke(c, size * 0.64, Stroke::new(1.6, t.error));
        }
        let body = if hovered == Some(i) {
            p.circle_stroke(c, size * 0.66, Stroke::new(1.5, hue));
            mix(body, Color32::WHITE, 0.15)
        } else {
            body
        };
        bot::paint(&p, c, size, body, &r.m.name);
        // A killed one wears a small x, at its top right.
        if r.m.killed {
            let b = c + vec2(size * 0.38, -size * 0.38);
            let rad = (size * 0.16).max(5.0);
            p.circle_filled(b, rad, t.panel);
            p.circle_stroke(b, rad, Stroke::new(1.0, t.ink3));
            let d = rad * 0.45;
            let x = Stroke::new(1.4, t.ink3);
            p.line_segment([b + vec2(-d, -d), b + vec2(d, d)], x);
            p.line_segment([b + vec2(-d, d), b + vec2(d, -d)], x);
        }
        let below = c0 + vec2(0.0, size_of(i, r) * 0.56 + 4.0);
        // On a backing of the ground, so the edges don't run through them.
        let backed = |at: Pos2, text: String, font: FontId, color: Color32| {
            let galley = p.layout_no_wrap(text, font, color);
            let r = Align2::CENTER_TOP.anchor_size(at, galley.size());
            p.rect_filled(r.expand2(vec2(3.0, 0.5)), 3.0, t.bg.gamma_multiply(0.82));
            p.galley(r.min, galley, color);
            r
        };
        let label = backed(below, format!("@{}", r.m.name), name_font.clone(), if r.m.killed { t.ink3 } else { t.ink });
        if z >= 0.6 {
            backed(egui::pos2(below.x, label.bottom() + 1.0), r.state.word().to_string(), state_font.clone(), state_color(t, &r.state));
        }
    }
    p.text(
        rect.left_top() + vec2(4.0, 4.0),
        Align2::LEFT_TOP,
        "Drag a member to move it, the background to pan; scroll to zoom; double-click to reset",
        FontId::proportional(11.0),
        t.ink4,
    );

    let animating = moving || fastest > 1.5 || g.drag.is_some() || !g.pulses.is_empty() || bg.dragged();
    if animating {
        ui.ctx().request_repaint();
    } else if rows.iter().any(|r| r.state == MemberState::Working && !r.m.killed) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
    ui.data_mut(|d| d.insert_temp(id, g));
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

