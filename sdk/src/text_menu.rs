//! The right-click menu every text field has: Cut, Copy, Paste and Select
//! all, with their shortcuts. egui's `TextEdit` has none, and a right press
//! moves its cursor, so `show` puts back the selection the press cleared;
//! while the menu is open the field keeps the keyboard (a click in the menu
//! would take it, ending edits that commit on losing it). Cut, Copy and
//! Paste go through egui's viewport commands, so they act as Ctrl+X, C and
//! V do (Paste brings pictures to the message box too). A secret field (a
//! masked password, key or token) offers neither Cut nor Copy.

use crate::egui::{
    self, Button, Id, Key, KeyboardShortcut, Modifiers, PointerButton, Popup, Rect, Response, TextEdit, Ui, UiBuilder,
    ViewportCommand, text::CCursor, text::CCursorRange, text_edit::TextEditOutput,
};

/// The field whose menu is open, in egui's temp data.
fn open_key() -> Id {
    Id::unique("text-menu-open")
}

/// Where the field's selection was before a right press moved it.
fn kept_key(field: Id) -> Id {
    field.with("text-menu-kept")
}

/// Adds `edit` with its menu.
pub fn add(ui: &mut Ui, edit: TextEdit) -> Response {
    show(ui, edit, false).response.response
}

/// Adds `edit`, masked when `secret` is (its menu then can't copy it out).
pub fn add_secret(ui: &mut Ui, edit: TextEdit, secret: bool) -> Response {
    show(ui, edit.password(secret), secret).response.response
}

/// `Ui::put` for a field: `edit` filling `rect`, with its menu.
pub fn put(ui: &mut Ui, rect: Rect, edit: TextEdit) -> Response {
    let layout = egui::Layout::centered_and_justified(egui::Direction::TopDown);
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(layout), |ui| add(ui, edit)).inner
}

/// Shows `edit` and its menu; `secret` when it is a masked field.
pub fn show(ui: &mut Ui, edit: TextEdit, secret: bool) -> TextEditOutput {
    let ctx = ui.ctx().clone();
    let open = ui.data(|d| d.get_temp::<Id>(open_key())).filter(|f| Popup::is_id_open(&ctx, f.with("popup")));
    // While a menu is open its field keeps the keyboard: a click in the
    // menu (or beside it, closing it) doesn't take it, and Escape closes
    // the menu without also leaving the field (egui drops the focus on
    // Escape before any widget runs).
    let before = ctx.options(|o| o.input_options.surrender_focus_on);
    if let Some(field) = open {
        ctx.options_mut(|o| o.input_options.surrender_focus_on = egui::SurrenderFocusOn::Never);
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            Popup::close_id(&ctx, field.with("popup"));
            ui.memory_mut(|m| m.request_focus(field));
        }
    }
    let out = edit.show(ui);
    if open.is_some() {
        ctx.options_mut(|o| o.input_options.surrender_focus_on = before);
    }
    menu(ui, &out, secret);
    out
}

fn menu(ui: &Ui, out: &TextEditOutput, secret: bool) {
    let resp = &out.response.response;
    let (ctx, id) = (ui.ctx().clone(), resp.id);
    let right = resp.secondary_clicked()
        || (resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.button_down(PointerButton::Secondary)));
    let open = Popup::is_id_open(&ctx, Popup::default_response_id(resp));
    if right {
        // The press put the cursor where it was: a selection made before
        // stays, for the menu to act on.
        if let Some(kept) = ui.data(|d| d.get_temp::<CCursorRange>(kept_key(id))).filter(|r| !r.is_empty())
            && let Some(mut state) = TextEdit::load_state(&ctx, id)
        {
            state.cursor.set_char_range(Some(kept));
            state.store(&ctx, id);
        }
    } else if !open {
        let range = out.cursor_range.filter(|_| ui.memory(|m| m.has_focus(id)));
        ui.data_mut(|d| match range {
            Some(range) => {
                d.insert_temp(kept_key(id), range);
            }
            None => d.remove::<CCursorRange>(kept_key(id)),
        });
    }
    if !right && !open {
        return;
    }
    let selection = TextEdit::load_state(&ctx, id).and_then(|s| s.cursor.char_range());
    let len = out.galley.text().chars().count();
    let selected = selection.is_some_and(|r| !r.is_empty());
    let everything = selection.is_some_and(|r| {
        let [a, b] = r.sorted_cursors();
        a.index.0 == 0 && b.index.0 == len
    });
    resp.context_menu(|ui| {
        ui.data_mut(|d| d.insert_temp(open_key(), id));
        let ctx = ui.ctx().clone();
        let shortcut = |key| ctx.format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, key));
        let items = [
            ("Cut", Key::X, selected && !secret, Some(ViewportCommand::RequestCut)),
            ("Copy", Key::C, selected && !secret, Some(ViewportCommand::RequestCopy)),
            ("Paste", Key::V, true, Some(ViewportCommand::RequestPaste)),
            ("Select all", Key::A, len > 0 && !everything, None),
        ];
        for (label, key, enabled, command) in items {
            if command.is_none() {
                ui.separator();
            }
            if !ui.add_enabled(enabled, Button::new(label).shortcut_text(shortcut(key))).clicked() {
                continue;
            }
            ui.memory_mut(|m| m.request_focus(id));
            match command {
                // The field gets it next frame, as the shortcut's event.
                Some(command) => ui.ctx().send_viewport_cmd(command),
                None => {
                    if let Some(mut state) = TextEdit::load_state(ui.ctx(), id) {
                        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(len))));
                        state.store(ui.ctx(), id);
                    }
                }
            }
            ui.ctx().request_repaint();
            ui.close();
        }
    });
}
