//! Selecting a whole message in the transcript. Egui's own text selection
//! works on the labels on screen, and its Ctrl+A selects only the label the
//! selection ends in (one paragraph of a reply). Here, Ctrl+A with a text
//! cursor or selection in a message (a reply, the user's message, a message from
//! another session) selects that message whole: it is lit as egui lights a
//! selection, and Ctrl+C copies its text as shown (`markdown::plain_text`),
//! the parts off screen too. A click outside its copy menu or Esc ends it.
//!
//! `begin` runs before the transcript is drawn (it takes the keys before the
//! labels see them); `message` draws each message, remembering which one a
//! selection started in and lighting the one selected.

use eframe::egui::{self, Event, Id, Key, LayerId, Modifiers, Shape, Ui, layers::ShapeIdx, text_selection::LabelSelectionState};

#[derive(Clone, Default)]
struct State {
    /// The message the latest press was in (where a selection starts).
    anchor: Option<Id>,
    /// Ctrl+A was pressed this frame: the anchor becomes selected when drawn.
    pending: bool,
    /// The message selected whole, its text (as of when it was last drawn)
    /// and the pass it was last drawn in.
    selected: Option<(Id, String, u64)>,
}

fn key(ctx: &egui::Context) -> Id {
    Id::unique(("oak-message-select", ctx.viewport_id()))
}

fn state(ctx: &egui::Context) -> State {
    let key = key(ctx);
    ctx.data(|d| d.get_temp(key)).unwrap_or_default()
}

fn set_state(ctx: &egui::Context, s: State) {
    let key = key(ctx);
    ctx.data_mut(|d| d.insert_temp(key, s));
}

/// The text of the message selected whole, if one is.
pub(crate) fn selected_text(ctx: &egui::Context) -> Option<String> {
    state(ctx).selected.map(|(_, text, _)| text).filter(|t| !t.is_empty())
}

/// Whether a message is selected whole (and lit).
pub(crate) fn any_selected(ctx: &egui::Context) -> bool {
    let key = key(ctx);
    ctx.data(|d| d.get_temp::<State>(key)).is_some_and(|s| s.selected.is_some())
}

fn skip_key(ctx: &egui::Context) -> Id {
    Id::unique(("oak-message-select-skip", ctx.viewport_id()))
}

/// What was painted since `from` (a code block's language and copy button)
/// isn't the message's text: it isn't lit when the message is selected.
pub(crate) fn not_text(ui: &Ui, from: ShapeIdx) {
    let end = ui.painter().add(Shape::Noop);
    let layer = ui.layer_id();
    let key = skip_key(ui.ctx());
    ui.ctx().data_mut(|d| d.get_temp_mut_or_default::<Vec<(LayerId, usize, usize)>>(key).push((layer, from.0, end.0)));
}

/// Before the transcript is drawn: a press or Esc ends a whole-message
/// selection, Ctrl+A in a message's selection starts one, Ctrl+C copies it.
pub(crate) fn begin(ui: &Ui) {
    let ctx = ui.ctx();
    let key = skip_key(ctx);
    ctx.data_mut(|d| d.remove_temp::<Vec<(LayerId, usize, usize)>>(key));
    let mut s = state(ctx);
    let pass = ctx.cumulative_pass_nr();
    s.pending = false;
    // (Gone from the screen, or another tab shown: nothing is lit any more.)
    if s.selected.as_ref().is_some_and(|(_, _, drawn)| drawn + 1 < pass) {
        s.selected = None;
    }
    let (pressed, escape) = ui.input(|i| (i.pointer.primary_pressed(), i.key_pressed(Key::Escape)));
    if pressed && !crate::copy_menu::selection_menu_pressed(ctx) {
        // A message drawn under the pointer sets it again. A press on
        // its copy menu acts on the existing selection and keeps it.
        s.anchor = None;
        s.selected = None;
    }
    if escape {
        s.selected = None;
    }
    let text_selected = ctx.plugin::<LabelSelectionState>().lock().has_selection();
    if s.anchor.is_some() && (text_selected || s.selected.is_some()) && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
        s.pending = true;
        ctx.plugin::<LabelSelectionState>().lock().clear_selection();
    }
    if let Some((_, text, _)) = &s.selected {
        let copy = ui.input_mut(|i| {
            let copy = i.events.iter().any(|e| matches!(e, Event::Copy | Event::Cut));
            i.events.retain(|e| !matches!(e, Event::Copy | Event::Cut));
            copy
        });
        if copy {
            ctx.copy_text(text.clone());
        }
    }
    set_state(ctx, s);
}

/// Draws a message (`draw`), `id` naming it across frames. `text` gives
/// what copying it whole copies.
pub(crate) fn message<R>(ui: &mut Ui, id: Id, text: impl FnOnce() -> String, draw: impl FnOnce(&mut Ui) -> R) -> R {
    let start = ui.painter().add(Shape::Noop);
    let inner = ui.scope(draw);
    let rect = inner.response.rect;
    let ctx = ui.ctx().clone();
    let mut s = state(&ctx);
    let mut changed = false;
    if ui.is_visible() && ui.input(|i| i.pointer.primary_pressed()) && ui.rect_contains_pointer(rect) {
        s.anchor = Some(id);
        changed = true;
    }
    if s.pending && s.anchor == Some(id) {
        s.pending = false;
        s.selected = Some((id, String::new(), 0));
    }
    if let Some((sel, copy, drawn)) = &mut s.selected
        && *sel == id
    {
        *copy = text();
        *drawn = ctx.cumulative_pass_nr();
        changed = true;
        light(ui, start);
    }
    if changed {
        set_state(&ctx, s);
    }
    inner.inner
}

/// Lights all the text painted since `start` as selected, as egui lights a
/// selection.
fn light(ui: &Ui, start: ShapeIdx) {
    let end = ui.painter().add(Shape::Noop);
    let visuals = ui.visuals().clone();
    let layer = ui.layer_id();
    let key = skip_key(ui.ctx());
    let skip = ui.ctx().data(|d| d.get_temp::<Vec<(LayerId, usize, usize)>>(key)).unwrap_or_default();
    ui.ctx().graphics_mut(|g| {
        let Some(list) = g.get_mut(layer) else { return };
        for i in start.0 + 1..end.0 {
            if skip.iter().any(|&(l, a, b)| l == layer && (a..b).contains(&i)) {
                continue;
            }
            list.mutate_shape(ShapeIdx(i), |s| {
                if let Shape::Text(text) = &mut s.shape {
                    let all = egui::text_selection::CCursorRange::select_all(&text.galley);
                    egui::text_selection::visuals::paint_text_selection(&mut text.galley, &visuals, &all, None);
                }
            });
        }
    });
}
