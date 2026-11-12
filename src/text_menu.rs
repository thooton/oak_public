//! The right-click menu every text field has (`oak_sdk::text_menu`, which
//! plugins use too); its tests are here, with the app's test harness.

pub(crate) use oak_sdk::text_menu::*;

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
