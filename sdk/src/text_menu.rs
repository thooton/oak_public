//! The right-click menu Oak's text fields have: Cut, Copy, Paste and
//! Select all, with their shortcuts. A secret field (a masked password,
//! key or token) offers neither Cut nor Copy.

use crate::abi::{OAK_EDIT_MENU, OAK_EDIT_PASSWORD, OAK_EDIT_SECRET};
use crate::egui::{Rect, Response, TextEdit, Ui};

/// Adds `edit` with its menu.
pub fn add(ui: &mut Ui, edit: TextEdit) -> Response {
    ui.add(edit.flag(OAK_EDIT_MENU, true))
}

/// Adds `edit`, masked when `secret` is (its menu then can't copy it out).
pub fn add_secret(ui: &mut Ui, edit: TextEdit, secret: bool) -> Response {
    ui.add(edit.flag(OAK_EDIT_MENU, true).flag(OAK_EDIT_PASSWORD | OAK_EDIT_SECRET, secret))
}

/// `Ui::put` for a field: `edit` filling `rect`, with its menu.
pub fn put(ui: &mut Ui, rect: Rect, edit: TextEdit) -> Response {
    ui.put(rect, edit.flag(OAK_EDIT_MENU, true))
}
