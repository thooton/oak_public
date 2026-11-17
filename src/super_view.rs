//! How a super agent looks, the one implementation of it: its chat as a
//! text conversation (people's messages in deep blue bubbles on the right,
//! the super agent's beside its avatar on the left), its corner box (what
//! it is doing, Stop, its model and its agents) and its agents' corner
//! boxes. A super agent on this computer (a conversation with the super
//! agent's role, `local_chat`) draws with it, and plugins draw the command
//! center's (the Oak tab's) through the plugin interface (`oak_super_chat`,
//! `oak_super_box`, `oak_super_agent_box`), each saying what to draw in JSON
//! (`Chat`, `SuperBox`, `AgentBox`).

use crate::look::{corner_area, corner_width, icon_font, panel_box};
use crate::theme::{Theme, mix};
use crate::time::{DAY_MS, clock12, day_clock, full_time, local_offset_ms};
use crate::widgets::{caption, clip_width, error_note, state_dot, text_width};
use eframe::egui::{self, Align2, Color32, FontId, Id, Rect, RichText, ScrollArea, Sense, Ui, vec2};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Nerd Font: a tree, Oak's super agents' icon.
pub(crate) const TREE_ICON: &str = "\u{f1bb}";

/// An outlined triangle, for Delta's agent (the text font supplies this glyph).
pub(crate) const DELTA_ICON: &str = "△";

/// A super agent's icon: Oak's tree, or Delta's triangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Icon {
    #[default]
    Tree,
    Delta,
}

impl Icon {
    /// The icon's glyph and font at about `size`.
    pub(crate) fn glyph(self, size: f32) -> (&'static str, FontId) {
        match self {
            Icon::Tree => (TREE_ICON, icon_font(size)),
            Icon::Delta => (DELTA_ICON, FontId::proportional(size)),
        }
    }
}

/// A super agent's chat, as it is drawn.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Chat {
    /// Keeps the chat's scroll apart from other chats'.
    pub(crate) salt: String,
    pub(crate) icon: Icon,
    /// Who "you" are: people's names show over their messages, but not yours.
    pub(crate) me: String,
    /// What an empty chat says.
    pub(crate) intro: String,
    /// False until the chat is there to show (a spinner, with `trouble`).
    pub(crate) loaded: bool,
    pub(crate) trouble: Option<String>,
    /// The super agent is at work, doing `activity`.
    pub(crate) working: bool,
    pub(crate) activity: String,
    /// It is writing a message to the chat now (a reply streaming in):
    /// three dots in a bubble by its avatar. At work otherwise (thinking,
    /// running tools), a spinner by what it is doing, no bubble.
    pub(crate) typing: bool,
    pub(crate) messages: Vec<Msg>,
}

/// A message in the chat.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Msg {
    /// "user" (people's, on the right), "oak" (the super agent's, on the
    /// left) or "error".
    pub(crate) kind: String,
    pub(crate) from: String,
    /// When it was sent; 0 while it is still going out, below 0 when
    /// that isn't known (from before Oak kept the time).
    pub(crate) time: i64,
    pub(crate) text: String,
    /// Keeps its markdown's state apart from other messages'.
    pub(crate) key: u64,
    pub(crate) images: Vec<Pic>,
}

/// An image in a message: shown (`block`, an image block), being fetched,
/// or one that couldn't be (`failed`, why).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Pic {
    pub(crate) name: String,
    pub(crate) block: Option<Value>,
    pub(crate) failed: Option<String>,
}

/// Google Messages' look: what people send in deep blue bubbles with white
/// text, on the right; the super agent's in neutral ones beside its
/// avatar, on the left.
const SENT: Color32 = Color32::from_rgb(0x0a, 0x2e, 0x7a);
const SENT_INK: Color32 = Color32::WHITE;
/// Messages from one sender this close together read as one stretch: tight,
/// their inner corners tucked in, the time once under the last.
const GROUP_MS: i64 = 5 * 60_000;
/// A pause this long puts the day and time over what follows.
const BREAK_MS: i64 = 30 * 60_000;
const AVATAR: f32 = 30.0;
const ROUND: u8 = 18;
const TUCK: u8 = 5;
/// How tall an image in a bubble is at most.
const IMAGE_H: f32 = 220.0;

/// The super agent's bubbles: the ink faintly over the page, so they stand
/// apart from it in either theme without taking a color.
fn received_fill(t: &Theme) -> Color32 {
    mix(t.bg, t.ink, 0.11)
}

/// Whether `b` carries on `a`'s stretch of messages.
fn same_stretch(a: &Msg, b: &Msg) -> bool {
    a.kind == b.kind && a.kind != "error" && a.from == b.from && (b.time <= 0 || (a.time > 0 && b.time - a.time < GROUP_MS))
}

/// Whether the day and time go over `b`, after `a`.
fn time_break(a: Option<&Msg>, b: &Msg, off: i64) -> bool {
    b.time > 0
        && match a {
            None => true,
            Some(a) => a.time > 0 && (b.time - a.time > BREAK_MS || (a.time + off).div_euclid(DAY_MS) != (b.time + off).div_euclid(DAY_MS)),
        }
}

/// A bubble's corners: round, but tucked in on the sender's side where it
/// joins the bubbles before and after it.
fn corners(right: bool, first: bool, last: bool) -> egui::CornerRadius {
    let (top, bottom) = (if first { ROUND } else { TUCK }, if last { ROUND } else { TUCK });
    if right {
        egui::CornerRadius { nw: ROUND, ne: top, sw: ROUND, se: bottom }
    } else {
        egui::CornerRadius { nw: top, ne: ROUND, sw: bottom, se: ROUND }
    }
}

/// A super agent's avatar: its icon on a disc, centered at `at`.
fn paint_avatar(p: &egui::Painter, t: &Theme, icon: Icon, at: egui::Pos2) {
    p.circle_filled(at, AVATAR / 2.0, mix(t.bg, t.accent, 0.22));
    let (glyph, font) = icon.glyph(16.0);
    p.text(at, Align2::CENTER_CENTER, glyph, font, t.accent);
}

/// The images someone sent, in their bubble over its text, each opening
/// in the viewer on a click.
fn sent_images(ui: &mut Ui, t: &Theme, pics: &[Pic]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for pic in pics {
            if let Some(b) = &pic.block {
                if let Some(r) = crate::images::show(ui, b, IMAGE_H)
                    && !pic.name.is_empty()
                {
                    r.on_hover_text(&pic.name);
                }
            } else if let Some(why) = &pic.failed {
                ui.label(RichText::new(format!("{} couldn't be shown", pic.name)).small().color(SENT_INK)).on_hover_text(why);
            } else {
                let (r, _) = ui.allocate_exact_size(vec2(IMAGE_H, IMAGE_H * 0.62), Sense::hover());
                ui.painter().rect_filled(r, 8.0, mix(SENT, t.bg, 0.25));
                ui.painter().text(r.center(), Align2::CENTER_CENTER, "Loading image…", FontId::proportional(11.5), SENT_INK);
            }
        }
    });
}

/// One message: a deep blue bubble on the right for people (their name
/// over the stretch when it isn't you), a neutral one on the left for the
/// super agent (its avatar by the stretch's last), and the time under the
/// stretch's last ("Sending…" while it goes out; every bubble has its full
/// date on hover).
fn bubble(ui: &mut Ui, t: &Theme, icon: Icon, m: &Msg, name: Option<&str>, first: bool, last: bool) {
    let right = m.kind == "user";
    let fill = if right { SENT } else { received_fill(t) };
    let frame = egui::Frame::new().fill(fill).corner_radius(corners(right, first, last)).inner_margin(egui::Margin::symmetric(14, 9));
    let indent = if right { 0.0 } else { AVATAR + 8.0 };
    let max_w = ((ui.available_width() - indent) * 0.78).max(120.0);
    let align = if right { egui::Align::Max } else { egui::Align::Min };
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if let Some(n) = name {
            ui.horizontal(|ui| {
                ui.add_space(indent + 6.0);
                ui.label(RichText::new(n).small().color(t.ink3));
            });
        }
        let shown = if right {
            ui.scope(|ui| {
                ui.set_max_width(max_w);
                frame
                    .show(ui, |ui| {
                        if !m.images.is_empty() {
                            sent_images(ui, t, &m.images);
                        }
                        if !m.text.is_empty() {
                            // Its lines start at the left: a bubble on the
                            // right doesn't right-align them.
                            ui.add(egui::Label::new(RichText::new(&m.text).color(SENT_INK)).wrap().halign(egui::Align::LEFT));
                        }
                    })
                    .response
            })
            .inner
        } else {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (slot, _) = ui.allocate_exact_size(vec2(AVATAR, 1.0), Sense::hover());
                let r = ui
                    .allocate_ui_with_layout(vec2(max_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        frame.show(ui, |ui| crate::transcript::markdown_highlighted(ui, t, &m.text, (0x5a9e7, m.key as usize))).response
                    })
                    .inner;
                if last {
                    paint_avatar(ui.painter(), t, icon, egui::pos2(slot.center().x, r.rect.bottom() - AVATAR / 2.0));
                }
                r
            })
            .inner
        };
        let id = Id::unique(("super-msg-menu", m.key, m.time, &m.text));
        crate::copy_menu::show(ui, id, shown.rect, Some(&|| m.text.clone()), None);
        if m.time > 0 {
            shown.on_hover_text(full_time(m.time, local_offset_ms()));
        }
        if last && m.time >= 0 {
            let when = if m.time > 0 { clock12(m.time, local_offset_ms()) } else { "Sending…".to_string() };
            ui.horizontal(|ui| {
                ui.add_space(indent + 6.0);
                ui.label(RichText::new(when).size(11.0).color(t.ink4));
            });
        }
    });
}

/// The super agent writing a message: three dots pulsing in a bubble of
/// its own, by its avatar, and what it is doing beside it.
fn typing(ui: &mut Ui, t: &Theme, icon: Icon, what: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (slot, _) = ui.allocate_exact_size(vec2(AVATAR, AVATAR + 4.0), Sense::hover());
        let (r, _) = ui.allocate_exact_size(vec2(58.0, 34.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(r, egui::CornerRadius { nw: ROUND, ne: ROUND, sw: TUCK, se: ROUND }, received_fill(t));
        let time = ui.input(|i| i.time);
        for k in 0..3 {
            let phase = (time * 4.0 - k as f64 * 0.7).sin() as f32 * 0.5 + 0.5;
            let at = egui::pos2(r.center().x + (k as f32 - 1.0) * 11.0, r.center().y - phase * 2.5);
            p.circle_filled(at, 3.6, mix(received_fill(t), t.ink2, 0.35 + 0.65 * phase));
        }
        paint_avatar(p, t, icon, egui::pos2(slot.center().x, r.bottom() - AVATAR / 2.0));
        ui.ctx().request_repaint();
        ui.label(RichText::new(what).small().color(t.ink3));
    });
}

/// The super agent at work on something other than a message (thinking,
/// running tools): a spinner and what it is doing, in the column its
/// messages start at, with no bubble, since no message is coming yet.
fn busy(ui: &mut Ui, t: &Theme, what: &str) {
    ui.horizontal(|ui| {
        ui.add_space(AVATAR + 14.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add(egui::Spinner::new().size(13.0).color(t.ink3));
        ui.label(RichText::new(what).small().color(t.ink3));
    });
}

/// The chat as a text conversation, in the transcript's column: people's
/// messages on the right, the super agent's (as markdown) on the left, in
/// stretches with the day and time over each after a pause, then whether
/// it is at work, then `tail` (a local super agent's questions).
pub(crate) fn chat(ui: &mut Ui, t: &Theme, c: &Chat, tail: impl FnOnce(&mut Ui)) {
    let (now, off) = (crate::api::now_ms(), local_offset_ms());
    crate::composer::chat_scroll(ui, t, c.salt.clone(), 0.0, |ui| {
        if !c.loaded {
            crate::widgets::loading(ui, t, c.trouble.as_deref());
            return;
        }
        if c.messages.is_empty() && !c.intro.is_empty() {
            caption(ui, &c.intro, t.ink3);
        }
        let msgs = &c.messages;
        for (i, m) in msgs.iter().enumerate() {
            let prev = i.checked_sub(1).map(|j| &msgs[j]);
            let next = msgs.get(i + 1);
            let broke = time_break(prev, m, off);
            if broke {
                ui.add_space(if i == 0 { 4.0 } else { 14.0 });
                ui.vertical_centered(|ui| ui.label(RichText::new(day_clock(m.time, now, off)).size(11.5).color(t.ink3)));
                ui.add_space(10.0);
            }
            let first = broke || prev.is_none_or(|p| !same_stretch(p, m));
            let last = next.is_none_or(|n| !same_stretch(m, n) || time_break(Some(m), n, off));
            if !first {
                ui.add_space(2.0);
            } else if !broke && i > 0 {
                ui.add_space(12.0);
            }
            if m.kind != "user" && m.kind != "oak" {
                ui.horizontal(|ui| {
                    ui.add_space(AVATAR + 14.0);
                    ui.vertical(|ui| error_note(ui, t, &m.text));
                });
                continue;
            }
            let name = (m.kind == "user" && first && m.from != c.me && !m.from.is_empty()).then_some(m.from.as_str());
            ui.scope(|ui| {
                if m.time == 0 {
                    ui.multiply_opacity(0.5);
                }
                bubble(ui, t, c.icon, m, name, first, last);
            });
        }
        if c.working {
            ui.add_space(12.0);
            let what = if c.activity.is_empty() { "Working…" } else { &c.activity };
            if c.typing { typing(ui, t, c.icon, what) } else { busy(ui, t, what) }
        }
        if let Some(e) = &c.trouble {
            ui.add_space(8.0);
            error_note(ui, t, e);
        }
        tail(ui);
        ui.add_space(16.0);
    });
}

/// A super agent's corner box, as it is drawn.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct SuperBox {
    /// Keeps the box's scroll and pickers apart from other boxes'.
    pub(crate) salt: String,
    pub(crate) icon: Icon,
    pub(crate) name: String,
    /// Under the name: where it runs, who shares it.
    pub(crate) caption: String,
    pub(crate) working: bool,
    /// What it is doing ("Working" while at work without one, else "Idle").
    pub(crate) activity: String,
    /// Each button's tip; empty: no such button.
    pub(crate) stop: String,
    pub(crate) clear: String,
    pub(crate) delete: String,
    /// The models it can run on, the one it does (none there: no pickers)
    /// at `effort`, and what the pickers say on hover.
    pub(crate) models: Vec<Model>,
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) model_tip: String,
    pub(crate) agents: Vec<AgentRow>,
}

/// A model in a super agent's pickers.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Model {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) efforts: Vec<String>,
}

/// One of a super agent's agents in its box.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct AgentRow {
    pub(crate) n: i64,
    pub(crate) title: String,
    /// What it is doing, at the row's right.
    pub(crate) state: String,
    /// Still going (dimmed when not), and at work now (a dot).
    pub(crate) live: bool,
    pub(crate) working: bool,
    /// Its last report, on hover.
    pub(crate) report: String,
    /// More on what it is doing (how long, its last progress), on hover
    /// under the report.
    pub(crate) detail: String,
}

/// What the user did in a super agent's box.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BoxAction {
    Stop,
    Clear,
    Delete,
    /// Run on this model at this effort.
    Model(String, String),
    /// Open this agent's tab.
    Open(i64),
}

/// A corner box's first rows: the super agent's icon, `title` and `caption`.
fn box_header(ui: &mut Ui, t: &Theme, icon: Icon, title: &str, caption_text: &str, w: f32) {
    ui.horizontal(|ui| {
        let (glyph, font) = icon.glyph(18.0);
        ui.label(RichText::new(glyph).font(font).color(t.accent));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(clip_width(ui, title, &FontId::proportional(14.0), w - 70.0)).strong().color(t.ink));
            if !caption_text.is_empty() {
                caption(ui, caption_text, t.ink3);
            }
        });
    });
}

/// What a corner box's agent is doing, in the accent colour while it works.
fn state_label(ui: &mut Ui, t: &Theme, state: &str, working: bool) {
    ui.label(RichText::new(state).small().color(if working { t.accent } else { t.ink2 }));
}

/// The effort to run model `o` at after switching to it: `effort` when it
/// has it, else medium, else its highest (Qwen's are low and high).
pub(crate) fn effort_for(o: &Model, effort: &str) -> String {
    if o.efforts.iter().any(|e| e == effort) {
        effort.to_string()
    } else if o.efforts.iter().any(|e| e == "medium") {
        "medium".to_string()
    } else {
        o.efforts.last().cloned().unwrap_or_else(|| "medium".to_string())
    }
}

/// The model and effort the super agent runs on.
fn model_pickers(ui: &mut Ui, b: &SuperBox, act: &mut Option<BoxAction>) {
    if let Some((m, e)) = pickers(ui, ("super", &b.salt), &b.models, &b.model, &b.effort, &b.model_tip) {
        *act = Some(BoxAction::Model(m, e));
    }
}

/// A model picker and an effort picker (when the model has efforts) for
/// `model` at `effort` of `models`, `tip` on hover: the model and effort
/// picked, if one was. Nothing when `model` isn't one of them.
fn pickers(ui: &mut Ui, salt: (&str, &str), models: &[Model], model: &str, effort: &str, tip: &str) -> Option<(String, String)> {
    let m = models.iter().find(|m| m.id == model)?;
    let mut picked = None;
    ui.horizontal(|ui| {
        let resp = egui::ComboBox::from_id_salt((salt, "model")).selected_text(RichText::new(&m.name).small()).show_ui(ui, |ui| {
            for o in models {
                if ui.selectable_label(o.id == model, &o.name).clicked() && o.id != model {
                    picked = Some((o.id.clone(), effort_for(o, effort)));
                }
            }
        });
        if !tip.is_empty() {
            resp.response.on_hover_text(tip);
        }
        if m.efforts.is_empty() {
            return;
        }
        let resp = egui::ComboBox::from_id_salt((salt, "effort")).selected_text(RichText::new(effort).small()).show_ui(ui, |ui| {
            for e in &m.efforts {
                if ui.selectable_label(e == effort, e).clicked() && e != effort {
                    picked = Some((model.to_string(), e.clone()));
                }
            }
        });
        if !tip.is_empty() {
            resp.response.on_hover_text(tip);
        }
    });
    picked
}

/// The super agent's agents, each a row that opens its tab (whose number
/// it returns).
fn agents_list(ui: &mut Ui, t: &Theme, w: f32, agents: &[AgentRow]) -> Option<i64> {
    if agents.is_empty() {
        return None;
    }
    let mut open = None;
    panel_box(ui, t, w, 8, 2.0, |ui| {
        caption(ui, "Agents", t.ink3);
        for a in agents {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(r, 0.0, t.tool_hover);
            }
            if a.working {
                state_dot(p, t, egui::pos2(r.left() + 5.0, r.center().y), false);
            }
            let font = FontId::proportional(12.5);
            let state_w = text_width(ui, &a.state, &FontId::proportional(11.5));
            let title = clip_width(ui, &format!("{}. {}", a.n, a.title), &font, r.width() - 22.0 - state_w);
            let p = ui.painter();
            p.text(egui::pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, title, font, if a.live { t.ink } else { t.ink3 });
            p.text(egui::pos2(r.right(), r.center().y), Align2::RIGHT_CENTER, &a.state, FontId::proportional(11.5), t.ink3);
            let hover = [a.report.as_str(), a.detail.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
            let resp = if hover.is_empty() { resp } else { resp.on_hover_text(hover) };
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open = Some(a.n);
            }
        }
    });
    open
}

/// A super agent's box in the corner of `area`: its icon, name and
/// caption, what it is doing (with Stop, Clear and Delete as `b` has them),
/// its model and effort, and its agents, each opening its tab. `more`
/// draws under it (a local super agent's notes and tasks).
pub(crate) fn super_box(ctx: &egui::Context, t: &Theme, area: Rect, b: &SuperBox, more: impl FnOnce(&mut Ui, f32)) -> Option<BoxAction> {
    let w = corner_width(area)?;
    let max_h = area.height() - 2.0 * crate::look::GAP;
    let mut act = None;
    corner_area(ctx, "super-box", area, |ui| {
        ui.set_max_height(max_h);
        ScrollArea::vertical().id_salt(("super-box", &b.salt)).max_height(max_h).auto_shrink([true, true]).show(ui, |ui| {
            panel_box(ui, t, w, 10, 4.0, |ui| {
                box_header(ui, t, b.icon, &b.name, &b.caption, w);
                let state = if !b.activity.is_empty() { &b.activity } else if b.working { "Working" } else { "Idle" };
                ui.horizontal(|ui| {
                    state_label(ui, t, state, b.working);
                    for (tip, what) in [(&b.stop, BoxAction::Stop), (&b.clear, BoxAction::Clear), (&b.delete, BoxAction::Delete)] {
                        let label = match what {
                            BoxAction::Stop => "Stop",
                            BoxAction::Clear => "Clear",
                            _ => "Delete",
                        };
                        if !tip.is_empty() && ui.small_button(label).on_hover_text(tip.as_str()).clicked() {
                            act = Some(what);
                        }
                    }
                });
                model_pickers(ui, b, &mut act);
            });
            if let Some(n) = agents_list(ui, t, w, &b.agents) {
                act = Some(BoxAction::Open(n));
            }
            more(ui, w);
        });
    });
    act
}

/// One of a super agent's agents' box, as it is drawn.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct AgentBox {
    pub(crate) icon: Icon,
    pub(crate) title: String,
    /// Under the title: which agent it is, whose.
    pub(crate) caption: String,
    pub(crate) working: bool,
    pub(crate) state: String,
    /// Stop's tip; empty: no Stop.
    pub(crate) stop: String,
    /// Its model and effort, in a line; empty: none. Shown when there are
    /// no pickers (`models` doesn't have `model_id`).
    pub(crate) model: String,
    /// The models it can be switched to, the one it runs on (`model_id`)
    /// at `effort`, and what the pickers say on hover.
    pub(crate) models: Vec<Model>,
    pub(crate) model_id: String,
    pub(crate) effort: String,
    pub(crate) model_tip: String,
    /// A switch asked for that waits for the request under way ("Switching
    /// to … after this step"), under the pickers; empty: none.
    pub(crate) pending: String,
    /// Pause's tip; empty: no Pause (or Resume, when `paused`).
    pub(crate) pause: String,
    pub(crate) paused: bool,
    /// A line on what it is doing (how long, its last progress), and what
    /// holds it up (in the error colour); empty: none.
    pub(crate) detail: String,
    pub(crate) blocker: String,
}

/// What the user did in an agent's box.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentAction {
    Stop,
    Pause,
    Resume,
    /// Run on this model at this effort.
    Model(String, String),
}

/// An agent's box in the corner of `area`: its title and whose it is,
/// what it is doing (with Stop, and Pause or Resume), its model (pickers
/// when it can be switched) and a switch waiting to happen. `more` draws
/// under it (a local agent's notes and tasks).
pub(crate) fn agent_box(ctx: &egui::Context, t: &Theme, area: Rect, b: &AgentBox, more: impl FnOnce(&mut Ui, f32)) -> Option<AgentAction> {
    let w = corner_width(area)?;
    let mut act = None;
    corner_area(ctx, "agent-box", area, |ui| {
        panel_box(ui, t, w, 10, 4.0, |ui| {
            box_header(ui, t, b.icon, &b.title, &b.caption, w);
            ui.horizontal(|ui| {
                state_label(ui, t, &b.state, b.working);
                if !b.stop.is_empty() && ui.small_button("Stop").on_hover_text(&b.stop).clicked() {
                    act = Some(AgentAction::Stop);
                }
                if !b.pause.is_empty() {
                    let (label, what) = if b.paused { ("Resume", AgentAction::Resume) } else { ("Pause", AgentAction::Pause) };
                    if ui.small_button(label).on_hover_text(&b.pause).clicked() {
                        act = Some(what);
                    }
                }
            });
            if !b.detail.is_empty() {
                caption(ui, &b.detail, t.ink3);
            }
            if !b.blocker.is_empty() {
                ui.add(egui::Label::new(RichText::new(&b.blocker).small().color(t.error)).wrap());
            }
            if let Some((m, e)) = pickers(ui, ("agent", &b.caption), &b.models, &b.model_id, &b.effort, &b.model_tip) {
                act = Some(AgentAction::Model(m, e));
            } else if !b.models.iter().any(|m| m.id == b.model_id) && !b.model.is_empty() {
                caption(ui, &b.model, t.ink3);
            }
            if !b.pending.is_empty() {
                ui.add(egui::Label::new(RichText::new(&b.pending).small().color(t.accent)).wrap());
            }
        });
        more(ui, w);
    });
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At work on something other than a message, the chat shows a spinner
    /// and what it is doing, not the typing bubble (its dots and avatar);
    /// writing a message, it shows the bubble.
    #[test]
    fn typing_dots_only_while_writing_a_message() {
        use crate::app_hover_test::{run_frame, test_ctx};
        let (ctx, t) = test_ctx(true, true);
        let frame = |typing: bool| {
            let c = Chat { loaded: true, working: true, typing, activity: if typing { "Writing" } else { "Thinking" }.into(), ..Default::default() };
            let mut time = 0.0;
            let mut out = None;
            for _ in 0..3 {
                out = Some(run_frame(&ctx, egui::vec2(900.0, 600.0), &mut time, 0.1, vec![], |ui| chat(ui, &t, &c, |_| {})));
            }
            let out = out.unwrap();
            let circles = out.shapes.iter().filter(|s| matches!(s.shape, egui::Shape::Circle(_))).count();
            (crate::app_hover_test::painted(&out).into_iter().map(|(s, _)| s).collect::<Vec<_>>(), circles)
        };
        let (shown, circles) = frame(false);
        assert!(shown.iter().any(|s| s == "Thinking"), "{shown:?}");
        assert_eq!(circles, 0, "no typing bubble while thinking");
        let (shown, circles) = frame(true);
        assert!(shown.iter().any(|s| s == "Writing"), "{shown:?}");
        assert!(circles >= 4, "the avatar and three dots: {circles}");
    }

    #[test]
    fn paused_super_agents_are_not_labelled_idle() {
        use crate::app_hover_test::{painted, run_frame, test_ctx};
        let (ctx, t) = test_ctx(true, true);
        let b = SuperBox { name: "Release train".into(), activity: "Paused".into(), ..Default::default() };
        let mut time = 0.0;
        let mut shown = vec![];
        for _ in 0..3 {
            shown = painted(&run_frame(&ctx, egui::vec2(1384.0, 821.0), &mut time, 0.1, vec![], |ui| {
                super_box(ui.ctx(), &t, ui.max_rect(), &b, |_, _| {});
            }));
        }
        assert!(shown.iter().any(|(s, _)| s == "Paused"), "{shown:?}");
        assert!(!shown.iter().any(|(s, _)| s == "Idle"), "{shown:?}");
    }
}

