//! Home's Plugins tab, the plugin manager: every library Oak found, with a
//! switch for each plugin, Enable all and Disable all, the plugins folder,
//! and Restart to apply once a switch differs from what runs (plugins load
//! only at startup, `plugins.rs`). A click on a row flips its switch;
//! switching one on switches on the plugins it needs.

use crate::look::{BAR_ROOM, list_rows, row_title_color};
use crate::plugins::{self, Found};
use crate::theme::Theme;
use crate::widgets::{clip_width, paint_switch};
use eframe::egui::{self, Align, Align2, Button, FontId, Layout, Rect, RichText, Ui, UiBuilder, vec2};

/// Home's tab id for the manager (not a plugin id, so no plugin's tab
/// takes its place).
pub(crate) const TAB: &str = "plugin-manager";
/// Nerd Font: a puzzle piece.
pub(crate) const ICON: &str = "\u{f12e}";

const HEAD_H: f32 = 28.0;
const SWITCH: egui::Vec2 = vec2(34.0, 18.0);

/// The id a row's switch acts on: a plugin's, when the library is one.
fn id_of(f: &Found) -> Option<&str> {
    f.info.as_ref().map(|i| i.id.as_str()).filter(|id| !id.is_empty())
}

/// It runs now.
fn running(f: &Found) -> bool {
    f.info.is_some() && !f.disabled && f.trouble.is_none()
}

/// Where Oak's own plugins folder is (made when opened).
fn folder() -> std::path::PathBuf {
    crate::store::dir().join("plugins")
}

/// The manager, in `area` (the left cell of Home).
pub(crate) fn show(ui: &mut Ui, t: &Theme, area: Rect) {
    let found = plugins::found();
    let head = Rect::from_min_max(egui::pos2(area.left() + 12.0, area.top() + 8.0), egui::pos2(area.right() - BAR_ROOM - 8.0, area.top() + 8.0 + HEAD_H));
    let list = Rect::from_min_max(egui::pos2(area.left(), head.bottom() + 6.0), area.max);
    head_row(ui, t, head, &found);
    ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
        if found.is_empty() {
            let note = if plugins::none_loaded() { "Started without plugins" } else { "No plugins found" };
            crate::widgets::center_note(ui, t, list, note);
            let sub = format!("Add libraries to {}", folder().display());
            ui.painter().text(list.center() + vec2(0.0, 20.0), Align2::CENTER_CENTER, sub, FontId::proportional(11.5), t.ink4);
            return;
        }
        list_rows(ui, t, "home-plugins", found.len(), "", |ui, i, rect, resp| row(ui, t, &found[i], rect, resp));
    });
}

/// The heading: how many run, then (from the right) the folder, Disable
/// all, Enable all and, when a switch changed, Restart to apply.
fn head_row(ui: &mut Ui, t: &Theme, head: Rect, found: &[Found]) {
    let ids: Vec<String> = found.iter().filter_map(id_of).map(str::to_string).collect();
    let on: Vec<&String> = ids.iter().filter(|id| plugins::enabled(id)).collect();
    let n = found.iter().filter(|f| f.info.is_some()).count();
    let run = found.iter().filter(|f| running(f)).count();
    ui.scope_builder(UiBuilder::new().max_rect(head).layout(Layout::right_to_left(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let small = |s: &str| RichText::new(s).size(12.0);
        let dirs = plugins::dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join("\n");
        if ui.button(small("Open folder")).on_hover_text(format!("Plugins are looked for in:\n{dirs}")).clicked() {
            let dir = folder();
            let _ = std::fs::create_dir_all(&dir);
            let _ = open::that_detached(&dir);
        }
        let all_off = on.is_empty();
        if ui.add_enabled(!all_off, Button::new(small("Disable all"))).clicked() {
            plugins::set_enabled(&ids, false);
        }
        if ui.add_enabled(on.len() < ids.len(), Button::new(small("Enable all"))).clicked() {
            plugins::set_enabled(&ids, true);
        }
        if plugins::pending() {
            let b = Button::new(RichText::new("Restart to apply").size(12.0).color(t.bg)).fill(t.accent);
            if ui.add(b).on_hover_text("Plugins load at startup. Active conversations pause for the restart, then resume.").clicked() {
                crate::update::restart(ui.ctx());
            }
        }
        let summary = if n == 0 && !found.is_empty() {
            format!("{} librar{}, none a plugin", found.len(), if found.len() == 1 { "y" } else { "ies" })
        } else {
            format!("{n} plugin{}, {run} running", crate::plural(n))
        };
        // What is left of the row, left aligned.
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let w = ui.available_width();
            let font = FontId::proportional(12.5);
            ui.label(RichText::new(clip_width(ui, &summary, &font, w)).font(font).color(t.ink3))
                .on_hover_text("A plugin runs with all of Oak's access to this computer: add only those you trust.");
        });
    });
}

/// One library's row: its name and version, what it is (or why it isn't
/// running), and its switch with what a restart will change.
fn row(ui: &mut Ui, t: &Theme, f: &Found, rect: Rect, resp: egui::Response) {
    let p = ui.painter().clone();
    let y = rect.center().y;
    let (x, right) = (rect.left() + 12.0, rect.right() - 12.0);
    let file = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let id = id_of(f).map(str::to_string);
    let on = id.as_deref().map(plugins::enabled);
    let lit = running(f) || on == Some(true);

    // The switch, and left of it what the next start does differently.
    let mut text_right = right;
    if let Some(on) = on {
        let sw = Rect::from_min_size(egui::pos2(right - SWITCH.x, y - SWITCH.y / 2.0), SWITCH);
        paint_switch(&p, t, sw, on, resp.hovered());
        text_right = sw.left() - 12.0;
        let was_on = !f.disabled;
        if on != was_on {
            let note = if on { "On after restart" } else { "Off after restart" };
            let font = FontId::proportional(11.5);
            let r = p.text(egui::pos2(text_right, y), Align2::RIGHT_CENTER, note, font, t.accent);
            text_right = r.left() - 12.0;
        }
    }

    // Name and version.
    let big = FontId::proportional(14.5);
    let name = f.info.as_ref().map_or(file.clone(), plugins::name_of);
    let color = if lit { row_title_color(t, resp.hovered()) } else { t.ink3 };
    let name = clip_width(ui, &name, &big, (text_right - x).max(0.0));
    let r = p.text(egui::pos2(x, y - 9.0), Align2::LEFT_CENTER, name, big, color);
    if let Some(info) = &f.info
        && !info.version.is_empty()
        && r.right() + 8.0 < text_right
    {
        let small = FontId::proportional(11.5);
        let v = clip_width(ui, &info.version, &small, text_right - r.right() - 8.0);
        p.text(egui::pos2(r.right() + 8.0, y - 8.0), Align2::LEFT_CENTER, v, small, t.ink4);
    }

    // What it is, or why it doesn't run.
    let off_needs: Vec<&str> = match (&f.info, on) {
        (Some(info), Some(true)) => info.needs.iter().filter(|n| !plugins::enabled(n)).map(String::as_str).collect(),
        _ => vec![],
    };
    let (line, line_color) = match (&f.trouble, &f.info) {
        _ if !off_needs.is_empty() => (format!("Needs {}, switched off", off_needs.join(", ")), t.error),
        (Some(why), _) => (format!("Not running: {why}"), t.error),
        (None, _) if f.disabled => (f.info.as_ref().map(|i| i.about.clone()).filter(|a| !a.is_empty()).unwrap_or_else(|| "Off".into()), t.ink4),
        (None, Some(info)) => (info.about.clone(), t.ink3),
        (None, None) => (String::new(), t.ink3),
    };
    let small = FontId::proportional(11.5);
    let line = clip_width(ui, &crate::look::one_line(&line), &small, (text_right - x).max(0.0));
    p.text(egui::pos2(x, y + 11.0), Align2::LEFT_CENTER, line, small, line_color);

    // The tooltip: everything about it, the trouble whole.
    let mut tip = vec![f.path.display().to_string()];
    if let Some(info) = &f.info {
        tip.push(format!("Id: {}", info.id));
        if !info.needs.is_empty() {
            tip.push(format!("Needs: {}", info.needs.join(", ")));
        }
    }
    if let Some(why) = &f.trouble {
        tip.push(format!("Not running: {why}"));
    }
    let resp = resp.on_hover_text(tip.join("\n"));
    resp.context_menu(|ui| {
        if ui.button("Open folder").clicked() {
            if let Some(dir) = f.path.parent() {
                let _ = open::that_detached(dir);
            }
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(f.path.display().to_string());
            ui.close();
        }
    });
    if resp.clicked()
        && let (Some(id), Some(on)) = (id, on)
    {
        plugins::set_enabled(&[id], !on);
    }
}
