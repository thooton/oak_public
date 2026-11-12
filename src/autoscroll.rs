//! Middle-click scrolling in a chat, as browsers do it: a transcript (whose
//! `vlist` scrolls itself) or a chat in an egui scroll area (`composer::
//! chat_scroll`: a plugin's chat). A middle press over one that
//! can scroll shows a ring where it was pressed; the further the pointer
//! goes above or below it, the faster it scrolls that way. Letting go of the
//! middle button after moving with it held ends it; after a plain click it
//! carries on until the next click (of any button), Esc, the wheel, or the
//! window losing focus.
//!
//! The presses and releases it takes never reach egui (this is a plugin's
//! input hook, like `copy_menu`'s), so the chat's text selection stays,
//! nothing under the pointer is clicked by the click that ends it, and a
//! middle click anywhere else (a tab, to close it) is egui's as usual.

use crate::Theme;
use crate::widgets::chevron_v;
use eframe::egui::{self, Context, CursorIcon, Event, Id, Key, LayerId, PointerButton, Pos2, Rect, Stroke, Ui, vec2};
use std::collections::HashMap;

/// How far the pointer goes, the middle button held, before letting go of
/// it ends the scrolling (a plain click leaves it on).
const DRAG: f32 = 12.0;

/// A chat that can scroll this way: its key, its layer and where it shows.
type Place = (u64, LayerId, Rect);

#[derive(Default)]
pub(crate) struct Autoscroll {
    // input_hook runs before egui enters the input viewport, so it uses
    // RawInput::viewport_id rather than Context::viewport_id.
    views: HashMap<egui::ViewportId, View>,
}

/// Scrolling under way: the list it scrolls and where it started.
#[derive(Clone, Copy, Debug, PartialEq)]
struct On {
    key: u64,
    anchor: Pos2,
    /// The middle button that started it is still down.
    held: bool,
    /// It has moved past `DRAG` while held: letting go ends it.
    dragged: bool,
}

#[derive(Default)]
struct View {
    /// The places registered last pass (where a middle press starts it)
    /// and this pass.
    last: Vec<Place>,
    this: Vec<Place>,
    /// Hit-testing as of the last pass (see `copy_menu`).
    hit: Option<Box<dyn Fn(Pos2) -> Option<LayerId> + Send + Sync>>,
    on: Option<On>,
    /// Buttons whose press was taken: their release is taken too.
    taken: Vec<PointerButton>,
    /// Where the pointer was last seen (it keeps scrolling from there when
    /// the pointer leaves the window).
    pointer: Option<Pos2>,
}

impl Autoscroll {
    fn view(&mut self, ctx: &Context) -> &mut View {
        self.views.entry(ctx.viewport_id()).or_default()
    }
}

impl View {
    /// The list whose place is topmost at `pos`.
    fn over(&self, pos: Pos2) -> Option<u64> {
        let top = self.hit.as_ref().and_then(|hit| hit(pos));
        self.last.iter().find(|(_, layer, r)| r.contains(pos) && top.is_none_or(|t| t == *layer)).map(|p| p.0)
    }

    fn input_hook(&mut self, input: &mut egui::RawInput) {
        let mut events = Vec::with_capacity(input.events.len());
        for e in input.events.drain(..) {
            match e {
                Event::PointerMoved(p) => {
                    self.pointer = Some(p);
                    if let Some(on) = &mut self.on
                        && on.held
                        && (p - on.anchor).length() > DRAG
                    {
                        on.dragged = true;
                    }
                    events.push(e);
                }
                Event::PointerButton { pos, button, pressed: true, .. } => {
                    // A release missed (outside the window) is not taken later.
                    self.taken.retain(|&b| b != button);
                    if self.on.is_some() {
                        // Any click ends it, and does nothing else.
                        self.on = None;
                        self.taken.push(button);
                    } else if button == PointerButton::Middle
                        && let Some(key) = self.over(pos)
                    {
                        self.on = Some(On { key, anchor: pos, held: true, dragged: false });
                        self.pointer = Some(pos);
                        self.taken.push(button);
                    } else {
                        events.push(e);
                    }
                }
                Event::PointerButton { button, pressed: false, .. } => {
                    if button == PointerButton::Middle
                        && let Some(on) = &mut self.on
                        && on.held
                    {
                        on.held = false;
                        if on.dragged {
                            self.on = None;
                        }
                    }
                    if let Some(k) = self.taken.iter().position(|&b| b == button) {
                        self.taken.remove(k);
                    } else {
                        events.push(e);
                    }
                }
                Event::Key { key: Key::Escape, pressed: true, .. } if self.on.is_some() => {
                    self.on = None;
                }
                Event::MouseWheel { .. } => {
                    self.on = None;
                    events.push(e);
                }
                Event::WindowFocused(false) => {
                    self.on = None;
                    events.push(e);
                }
                e => events.push(e),
            }
        }
        input.events = events;
    }

    fn on_end_pass(&mut self, ctx: &Context) {
        // Its transcript isn't shown any more (another tab, a closed pane).
        if let Some(on) = self.on
            && !self.this.iter().any(|p| p.0 == on.key)
        {
            self.on = None;
        }
        if let (Some(on), Some(p)) = (self.on, self.pointer) {
            // Last, over the text cursor of the labels it goes past.
            let v = (p.y - on.anchor.y) / DRAG;
            ctx.set_cursor_icon(match v {
                v if v < -1.0 => CursorIcon::ResizeNorth,
                v if v > 1.0 => CursorIcon::ResizeSouth,
                _ => CursorIcon::AllScroll,
            });
        }
        self.last = std::mem::take(&mut self.this);
        let areas = ctx.memory(|m| m.areas().clone());
        let transforms = ctx
            .memory(|m| m.layer_ids().collect::<Vec<_>>())
            .into_iter()
            .filter_map(|layer| ctx.layer_transform_to_global(layer).map(|transform| (layer, transform)))
            .collect();
        self.hit = Some(Box::new(move |pos| areas.layer_id_at(pos, &transforms)));
    }
}

impl egui::Plugin for Autoscroll {
    fn debug_name(&self) -> &'static str {
        "autoscroll"
    }

    fn input_hook(&mut self, _ctx: &Context, input: &mut egui::RawInput) {
        self.views.entry(input.viewport_id).or_default().input_hook(input);
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.view(&ctx).on_end_pass(&ctx);
    }
}

pub(crate) fn install(ctx: &Context) {
    ctx.add_plugin(Autoscroll::default());
}

/// The chat `key` shows in `view` (in `ui`'s layer); a middle press there
/// starts scrolling it when it can scroll (`scrollable`). While it is on,
/// shows the ring where it started and gives how far to scroll it this
/// frame (down positive): Firefox's curve, (distance / 12)^1.5 - 1 points
/// every 20 ms, past the ring.
pub(crate) fn scroll(ui: &Ui, t: &Theme, key: u64, view: Rect, scrollable: bool) -> Option<f32> {
    let (anchor, p) = place(ui, key, view, scrollable)?;
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    let v = (p.y - anchor.y) / DRAG;
    let per_20ms = if v > 1.0 { v * v.sqrt() - 1.0 } else if v < -1.0 { v * (-v).sqrt() + 1.0 } else { 0.0 };
    ui.ctx().request_repaint();
    // The anchor: a ring with arrows up and down.
    let painter = ui.ctx().layer_painter(LayerId::new(egui::Order::Foreground, Id::unique("autoscroll")));
    painter.circle(anchor, 13.0, t.panel.gamma_multiply(0.9), Stroke::new(1.0, t.ink3));
    painter.circle_filled(anchor, 2.0, t.ink2);
    chevron_v(&painter, anchor + vec2(0.0, -6.5), true, t.ink2);
    chevron_v(&painter, anchor + vec2(0.0, 6.5), false, t.ink2);
    Some(per_20ms * dt / 0.020)
}

fn place(ui: &Ui, key: u64, view: Rect, scrollable: bool) -> Option<(Pos2, Pos2)> {
    let ctx = ui.ctx();
    let m = ctx.plugin_opt::<Autoscroll>()?;
    let mut guard = m.lock();
    let v = guard.view(ctx);
    let shown = view.intersect(ui.clip_rect());
    let on = v.on.filter(|on| on.key == key);
    if (scrollable || on.is_some()) && shown.is_positive() {
        v.this.push((key, ui.layer_id(), shown));
    }
    Some((on?.anchor, v.pointer?))
}
