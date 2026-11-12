//! Renaming a conversation: a small dialog, opened from its tab's or its
//! Home row's right-click menu, or from its title in the session box
//! (double-click or right-click). The name becomes the conversation's title;
//! its id and history stay, and the next save renames its file after it.
//! A title still being made (`agent::Session::name_session`) no longer
//! replaces it. Subagents keep theirs: their sessions address them by it.

use super::*;

/// The conversation being renamed: its id, the name being typed, and
/// whether the field still has to be focused (with the name selected).
pub(crate) struct Renaming {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) focus: bool,
}

/// The longest name kept, in characters.
const MAX_NAME: usize = 200;

/// Sets `c`'s title to `name` (one line, trimmed, at most `MAX_NAME`
/// characters) and marks it titled. False, changing nothing, when the name
/// is empty or the title already.
pub(crate) fn apply_rename(c: &mut store::Conversation, name: &str) -> bool {
    let name: String = crate::tools::one_line(name).trim().head(MAX_NAME);
    if name.is_empty() || (name == c.title && c.titled) {
        return false;
    }
    c.title = name;
    c.titled = true;
    true
}

impl App {
    /// Conversation `id` can be renamed: one on this machine that isn't a
    /// subagent.
    pub(crate) fn can_rename(&self, id: &str) -> bool {
        self.sessions.get(id).is_some_and(|s| s.conv.lock().unwrap().parent.is_none())
    }

    /// Opens the rename dialog for conversation `id`.
    pub(crate) fn start_rename(&mut self, id: &str) {
        if !self.can_rename(id) {
            return;
        }
        let name = self.sessions[id].conv.lock().unwrap().title.clone();
        self.renaming = Some(Renaming { id: id.to_string(), name, focus: true });
    }

    /// The rename dialog, while one is open: Enter or Rename saves, Esc,
    /// Cancel or a click outside leaves the name as it was.
    pub(crate) fn rename_dialog(&mut self, ctx: &egui::Context, t: &Theme) {
        let Some(r) = &mut self.renaming else { return };
        let Some(s) = self.sessions.get(&r.id) else {
            self.renaming = None;
            return;
        };
        let field = Id::unique("rename-field");
        let mut choice = None;
        let modal = crate::widgets::dialog("rename-dialog", t).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new("Rename conversation").strong().color(t.ink));
            ui.add_space(10.0);
            let resp = crate::text_menu::add(ui, TextEdit::singleline(&mut r.name).id(field).char_limit(MAX_NAME).hint_text("Name").desired_width(f32::INFINITY));
            if std::mem::take(&mut r.focus) {
                resp.request_focus();
                if let Some(mut state) = TextEdit::load_state(ui.ctx(), field) {
                    let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(r.name.chars().count()));
                    state.cursor.set_char_range(Some(all));
                    state.store(ui.ctx(), field);
                }
            }
            let ok = !r.name.trim().is_empty();
            // A single-line field lets go of the keyboard on Enter.
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                choice = Some(ok);
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(ok, Button::new(RichText::new("Rename").color(t.accent))).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(false);
        }
        let Some(save) = choice else { return };
        if save {
            let mut c = s.conv.lock().unwrap();
            if apply_rename(&mut c, &r.name) {
                c.save();
            }
        }
        self.renaming = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rename sets the title and keeps the id and messages; the file is
    /// renamed after it and the old one goes.
    #[test]
    fn rename_keeps_the_conversation() {
        let mut c = store::Conversation { id: format!("unit-test-rename-{}", api::now_ms()), title: "First line of the message".into(), ..Default::default() };
        c.messages.push(serde_json::json!({"role": "user", "content": "Fix the parser"}));
        c.save();
        store::flush();
        let old = store::conversations_dir().join(c.file_name());
        assert!(old.is_file());
        assert!(!apply_rename(&mut c, "  \n "), "an empty name changes nothing");
        assert!(apply_rename(&mut c, "  Parser\nwork  "));
        assert_eq!((c.title.as_str(), c.titled, c.messages.len()), ("Parser work", true, 1));
        assert!(!apply_rename(&mut c, "Parser work"), "the same name changes nothing");
        c.save();
        store::flush();
        assert!(!old.exists(), "the old file goes");
        let loaded = store::load_conversations().into_iter().find(|l| l.id == c.id).unwrap();
        assert_eq!((loaded.title.as_str(), loaded.titled, loaded.messages.len()), ("Parser work", true, 1));
        store::remove_conversation(&c.id);
    }

    /// A long name is cut to `MAX_NAME` characters.
    #[test]
    fn rename_caps_long_names() {
        let mut c = store::Conversation::default();
        assert!(apply_rename(&mut c, &"é".repeat(MAX_NAME + 50)));
        assert_eq!(c.title.chars().count(), MAX_NAME);
    }
}
