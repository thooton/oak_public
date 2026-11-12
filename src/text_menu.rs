//! The right-click menu every text field has: Cut, Copy, Paste and Select
//! all, with their shortcuts. egui's `TextEdit` has none, and a right press
//! moves its cursor, so `show` puts back the selection the press cleared;
//! while the menu is open the field keeps the keyboard (a click in the menu
//! would take it, ending edits that commit on losing it). Cut, Copy and
//! Paste go through egui's viewport commands, so they act as Ctrl+X, C and
//! V do (Paste brings pictures to the message box too). A secret field (a
//! masked password, key or token) offers neither Cut nor Copy.

use eframe::egui::{
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{click, painted, rect_of, run_frame};
    use eframe::egui::{Context, Event, Id, Key, Modifiers, PointerButton, Popup, Pos2, Rect, TextEdit, ViewportCommand, ViewportId, pos2, text::CCursor, text::CCursorRange, vec2};

    struct Field {
        ctx: Context,
        text: String,
        secret: bool,
        time: f64,
    }

    impl Field {
        fn id() -> Id {
            Id::unique("text-menu-test")
        }

        /// One frame: the commands it sent and where each menu item is.
        fn run(&mut self, events: Vec<Event>) -> (Vec<ViewportCommand>, Vec<(String, Rect)>) {
            let (text, secret) = (&mut self.text, self.secret);
            let mut out = run_frame(&self.ctx, vec2(600.0, 400.0), &mut self.time, 1.0 / 60.0, events, |ui| {
                add_secret(ui, TextEdit::singleline(text).id(Field::id()).desired_width(300.0), secret);
            });
            let items = painted(&out);
            (out.viewport_output.remove(&ViewportId::ROOT).map(|v| v.commands).unwrap_or_default(), items)
        }

        /// A click, a frame per event, and a frame after it.
        fn click(&mut self, at: Pos2, button: PointerButton) -> (Vec<ViewportCommand>, Vec<(String, Rect)>) {
            let mut commands = vec![];
            for event in click(at, button) {
                commands.extend(self.run(vec![event]).0);
            }
            let (more, items) = self.run(vec![]);
            commands.extend(more);
            (commands, items)
        }

        fn selection(&self) -> Option<(usize, usize)> {
            let r = TextEdit::load_state(&self.ctx, Field::id())?.cursor.char_range()?;
            let [a, b] = r.sorted_cursors();
            Some((a.index.0, b.index.0))
        }

        fn select(&mut self, from: usize, to: usize) {
            let mut state = TextEdit::load_state(&self.ctx, Field::id()).unwrap();
            state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(from), CCursor::new(to))));
            state.store(&self.ctx, Field::id());
            self.run(vec![]);
        }

        fn focused(&self) -> bool {
            self.ctx.memory(|m| m.has_focus(Field::id()))
        }
    }

    fn field(secret: bool) -> Field {
        let mut f = Field { ctx: Context::default(), text: "hello world".into(), secret, time: 0.0 };
        f.run(vec![]);
        f.click(pos2(20.0, 10.0), PointerButton::Primary);
        assert!(f.focused());
        f.select(6, 11);
        f
    }

    /// A right-click keeps the selection and opens the menu; Copy asks for
    /// the copy, keeping the field's focus and selection; Select all selects
    /// all of it.
    #[test]
    fn menu_keeps_the_selection_and_acts_on_it() {
        let mut f = field(false);
        let (_, items) = f.click(pos2(15.0, 10.0), PointerButton::Secondary);
        assert_eq!(f.selection(), Some((6, 11)), "the right press kept the selection");
        for label in ["Cut", "Copy", "Paste", "Select all"] {
            rect_of(&items, label);
        }
        let (commands, _) = f.click(rect_of(&items, "Copy").center(), PointerButton::Primary);
        assert!(commands.iter().any(|c| matches!(c, ViewportCommand::RequestCopy)), "{commands:?}");
        assert!(f.focused(), "the field kept the keyboard");
        assert_eq!(f.selection(), Some((6, 11)));
        let (_, items) = f.click(pos2(15.0, 10.0), PointerButton::Secondary);
        f.click(rect_of(&items, "Select all").center(), PointerButton::Primary);
        assert_eq!(f.selection(), Some((0, 11)));
        // Escape closes the menu, not the field.
        let (_, items) = f.click(pos2(15.0, 10.0), PointerButton::Secondary);
        rect_of(&items, "Paste");
        let key = |pressed| Event::Key { key: Key::Escape, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
        f.run(vec![key(true), key(false)]);
        let (_, items) = f.run(vec![]);
        assert!(!items.iter().any(|(t, _)| t == "Paste"), "the menu closed");
        assert!(f.focused(), "Escape left the field focused");
    }

    /// A masked field's menu can't copy it out: Cut and Copy do nothing.
    #[test]
    fn secret_fields_neither_cut_nor_copy() {
        let mut f = field(true);
        for label in ["Cut", "Copy"] {
            let (_, items) = f.click(pos2(15.0, 10.0), PointerButton::Secondary);
            let (commands, _) = f.click(rect_of(&items, label).center(), PointerButton::Primary);
            assert!(commands.iter().all(|c| !matches!(c, ViewportCommand::RequestCopy | ViewportCommand::RequestCut)), "{label}: {commands:?}");
            assert_eq!(f.text, "hello world");
            Popup::close_id(&f.ctx, Field::id().with("popup"));
        }
        let (_, items) = f.click(pos2(15.0, 10.0), PointerButton::Secondary);
        let (commands, _) = f.click(rect_of(&items, "Paste").center(), PointerButton::Primary);
        assert!(commands.iter().any(|c| matches!(c, ViewportCommand::RequestPaste)), "{commands:?}");
    }
}
