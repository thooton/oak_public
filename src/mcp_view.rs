//! Home's Protocols tab: the programs conversations can work in (`mcp.rs`:
//! MCP servers, as models know them, and Oak's Browser and Window kits),
//! Oak's own first, then those of the files in `protocols/`, each with its
//! switch, its connection's state and what it offers. The heading has the
//! folder and Reload (connections whose file changed are ended, to be made
//! again on next use). Blender's row says whether Blender is reachable and
//! has Install add-on; Browser's says whether Oak's extension in the user's
//! own browser is connected; right-click a row for the rest (Connect, its
//! file or log, for Blender how to use it from Claude Code or Codex, for
//! Browser Set up browser extension).

use crate::look::{BAR_ROOM, list_rows, row_title_color};
use crate::mcp::{self, Config, Status};
use crate::theme::Theme;
use crate::widgets::{clip_width, paint_switch};
use eframe::egui::{self, Align, Align2, Button, FontId, Layout, Rect, RichText, Ui, UiBuilder, vec2};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Home's tab id.
pub(crate) const TAB: &str = "protocols";
/// Nerd Font: a plug.
pub(crate) const ICON: &str = "\u{f1e6}";

const HEAD_H: f32 = 28.0;
const SWITCH: egui::Vec2 = vec2(34.0, 18.0);

/// The outcome of the last action (install, connect, reload), shown in the
/// heading for a while: (when, text, failed).
static LAST: Mutex<Option<(Instant, String, bool)>> = Mutex::new(None);
const SHOW_OUTCOME: Duration = Duration::from_secs(10);
/// Something running in the background (installing, connecting): its label.
static BUSY: Mutex<Option<String>> = Mutex::new(None);

fn said(text: String, failed: bool) {
    *LAST.lock().unwrap() = Some((Instant::now(), text, failed));
}

/// Runs `f` off the UI thread, its outcome shown in the heading.
fn in_background(ctx: &egui::Context, label: &str, f: impl FnOnce() -> Result<String, String> + Send + 'static) {
    let mut busy = BUSY.lock().unwrap();
    if busy.is_some() {
        return;
    }
    *busy = Some(label.to_string());
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let r = f();
        *BUSY.lock().unwrap() = None;
        match r {
            Ok(s) => said(s, false),
            Err(e) => said(e, true),
        }
        ctx.request_repaint();
    });
}

/// The tab, in `area` (the left cell of Home).
pub(crate) fn show(ui: &mut Ui, t: &Theme, area: Rect) {
    // Connections and Blender come and go on their own.
    ui.ctx().request_repaint_after(Duration::from_secs(2));
    crate::blender_view::watch(ui.ctx());
    let servers = mcp::configs();
    let head = Rect::from_min_max(egui::pos2(area.left() + 12.0, area.top() + 8.0), egui::pos2(area.right() - BAR_ROOM - 8.0, area.top() + 8.0 + HEAD_H));
    let list = Rect::from_min_max(egui::pos2(area.left(), head.bottom() + 6.0), area.max);
    head_row(ui, t, head, &servers);
    ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
        list_rows(ui, t, "home-mcp", servers.len(), "", |ui, i, rect, resp| row(ui, t, &servers[i], rect, resp));
    });
}

fn head_row(ui: &mut Ui, t: &Theme, head: Rect, servers: &[Config]) {
    ui.scope_builder(UiBuilder::new().max_rect(head).layout(Layout::right_to_left(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let small = |s: &str| RichText::new(s).size(12.0);
        let tip = format!(
            "Each protocol is a JSON file in {}, as Claude Code writes MCP servers: {{\"command\": \"npx\", \"args\": [...]}} or {{\"type\": \"http\", \"url\": \"...\"}}. A .mcp.json or Claude Desktop config with \"mcpServers\" works as it is.",
            mcp::folder().display()
        );
        if ui.button(small("Open folder")).on_hover_text(tip).clicked() {
            let dir = mcp::folder();
            let _ = std::fs::create_dir_all(&dir);
            let _ = open::that_detached(&dir);
        }
        if ui.button(small("Reload")).on_hover_text("Reads the folder again; protocols whose file changed restart on their next use.").clicked() {
            let n = mcp::reload();
            said(if n == 0 { "Reloaded".into() } else { format!("Reloaded; {n} connection{} ended", crate::plural(n)) }, false);
        }
        let busy = BUSY.lock().unwrap().clone();
        let last = LAST.lock().unwrap().clone().filter(|(at, ..)| at.elapsed() < SHOW_OUTCOME);
        let (summary, color, full) = if let Some(b) = busy {
            (format!("{b}..."), t.ink3, String::new())
        } else if let Some((_, text, failed)) = last {
            (crate::look::one_line(&text), if failed { t.error } else { t.accent }, text)
        } else {
            let on = servers.iter().filter(|c| c.trouble.is_none() && mcp::enabled(&c.name)).count();
            let up = servers.iter().filter(|c| mcp::status(&c.name) == Status::Ready).count();
            (format!("{} protocol{}, {on} on, {up} connected", servers.len(), crate::plural(servers.len())), t.ink3, String::new())
        };
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let w = ui.available_width();
            let font = FontId::proportional(12.5);
            let r = ui.label(RichText::new(clip_width(ui, &summary, &font, w)).font(font).color(color));
            let tip = if full.is_empty() { "A protocol runs with your access to this computer: add only those you trust. Conversations load one when a task needs it.".to_string() } else { full };
            r.on_hover_text(tip);
        });
    });
}

fn row(ui: &mut Ui, t: &Theme, c: &Config, rect: Rect, resp: egui::Response) {
    let p = ui.painter().clone();
    let y = rect.center().y;
    let (x, right) = (rect.left() + 12.0, rect.right() - 12.0);
    let on = mcp::enabled(&c.name);
    let usable = c.trouble.is_none();
    let status = mcp::status(&c.name);
    let blender = c.builtin() && c.name == mcp::blender::NAME;

    // The switch, the state left of it, and Blender's button.
    let mut text_right = right;
    if usable {
        let sw = Rect::from_min_size(egui::pos2(right - SWITCH.x, y - SWITCH.y / 2.0), SWITCH);
        paint_switch(&p, t, sw, on, resp.hovered());
        text_right = sw.left() - 12.0;
        let note = match &status {
            Status::Ready if on => Some(("Connected", t.accent)),
            Status::Connecting => Some(("Starting", t.ink3)),
            Status::Failed(_) if on => Some(("Failed", t.error)),
            _ => None,
        };
        if let Some((note, color)) = note {
            let r = p.text(egui::pos2(text_right, y), Align2::RIGHT_CENTER, note, FontId::proportional(11.5), color);
            text_right = r.left() - 12.0;
        }
    }
    let mut install_clicked = false;
    if blender && on {
        let installed = !mcp::blender::installed_addons().is_empty();
        let label = if !installed { Some("Install add-on") } else if mcp::blender::addon_outdated() { Some("Update add-on") } else { None };
        if let Some(label) = label {
            let size = vec2(104.0, 22.0);
            let b = Rect::from_min_size(egui::pos2(text_right - size.x, y - size.y / 2.0), size);
            let busy = BUSY.lock().unwrap().is_some();
            install_clicked = ui
                .put(b, Button::new(RichText::new(label).size(12.0)))
                .on_hover_text("Puts Oak's add-on into Blender's add-ons and switches it on (Blender stays closed while it does). Then open Blender: the add-on listens on port 9876.")
                .clicked()
                && !busy;
            text_right = b.left() - 10.0;
        }
    }

    // Name, and what serves it.
    let big = FontId::proportional(14.5);
    let lit = usable && on;
    let color = if lit { row_title_color(t, resp.hovered()) } else { t.ink3 };
    let name = clip_width(ui, &c.title, &big, (text_right - x).max(0.0));
    let r = p.text(egui::pos2(x, y - 9.0), Align2::LEFT_CENTER, name, big, color);
    let offer = mcp::offer(c);
    let n_tools = offer.as_ref().map(|o| o.tools.len());
    let tag = match (c.builtin(), n_tools) {
        _ if c.native() => format!("built in, {}", c.family),
        (true, Some(n)) => format!("built in, {n} tools"),
        (false, Some(n)) => format!("{n} tool{}", crate::plural(n)),
        _ => String::new(),
    };
    if !tag.is_empty() && r.right() + 8.0 < text_right {
        let small = FontId::proportional(11.5);
        let v = clip_width(ui, &tag, &small, text_right - r.right() - 8.0);
        p.text(egui::pos2(r.right() + 8.0, y - 8.0), Align2::LEFT_CENTER, v, small, t.ink4);
    }

    // The line under it: trouble, failure, Blender's state, or the target.
    let (line, line_color) = if let Some(why) = &c.trouble {
        (format!("Not usable: {why}"), t.error)
    } else if !on {
        ("Off: conversations can't load it".to_string(), t.ink4)
    } else if let Status::Failed(e) = &status {
        (e.clone(), t.error)
    } else if blender {
        blender_line(t)
    } else if c.native() && c.family == "BROWSER" {
        match crate::browser::bridge::connected() {
            Some(b) => (format!("Oak's own; its extension in {b} is connected: conversations can ask for your tabs"), t.accent),
            None => ("Oak's own; right-click to set up the extension that lets it use your own browser's tabs".into(), t.ink3),
        }
    } else if c.native() {
        (format!("Oak's own: conversations load it as {}", c.family), t.ink3)
    } else {
        (c.target(), t.ink3)
    };
    let small = FontId::proportional(11.5);
    let line = clip_width(ui, &crate::look::one_line(&line), &small, (text_right - x).max(0.0));
    p.text(egui::pos2(x, y + 11.0), Align2::LEFT_CENTER, line, small, line_color);

    // The tooltip: everything about it.
    let mut tip = vec![];
    if !c.description.is_empty() {
        tip.push(c.description.clone());
    }
    tip.push(c.target());
    if let Some(f) = &c.file {
        tip.push(f.display().to_string());
    }
    if let Some(o) = &offer {
        if !o.server.is_empty() {
            tip.push(format!("Server: {}", o.server));
        }
        if !o.tools.is_empty() {
            let names: Vec<&str> = o.tools.iter().filter_map(|t| t["name"].as_str()).collect();
            tip.push(format!("Tools: {}", names.join(", ")));
        }
        if !c.builtin() && o.at_ms > 0 {
            tip.push(format!("Listed {} ago", mcp::age(o.at_ms)));
        }
    }
    if let Status::Failed(e) = &status {
        tip.push(format!("Last connection: {e}"));
    }
    if let Some(why) = &c.trouble {
        tip.push(why.clone());
    }
    let resp = resp.on_hover_text(tip.join("\n"));
    let ctx = ui.ctx().clone();
    resp.context_menu(|ui| menu(ui, c, usable && on, &ctx));
    if install_clicked {
        install(&ctx);
    } else if resp.clicked() && usable {
        mcp::set_enabled(std::slice::from_ref(&c.name), !on);
    }
}

/// What Blender's row says about Blender.
fn blender_line(t: &Theme) -> (String, egui::Color32) {
    let installed = mcp::blender::installed_addons();
    let found = !mcp::blender::find_blender().is_empty();
    if mcp::blender_run::running_count() > 0 {
        return (format!("{} private agent Blender instances running", mcp::blender_run::running_count()), t.accent);
    }
    match crate::blender_view::reachable() {
        Some(true) => ("Your Blender is available for explicit attachment; agents start private instances".into(), t.accent),
        _ if !found => ("Install Blender (blender.org) to let conversations work in it".into(), t.ink3),
        // Oak starts its own when a conversation needs one.
        _ if cfg!(windows) && installed.is_empty() => ("Ready; Install add-on to work in your own Blender too".into(), t.ink3),
        _ if cfg!(windows) => ("Ready; each agent loads its own hidden Blender".into(), t.ink3),
        _ if installed.is_empty() => ("Install the add-on to let conversations work in Blender".into(), t.ink3),
        _ => ("Add-on installed; open Blender to connect".into(), t.ink3),
    }
}

fn install(ctx: &egui::Context) {
    in_background(ctx, "Installing the Blender add-on", mcp::blender::install_addon);
}

/// Writes Oak's browser extension to Oak's folder (`browser-extension`) and
/// opens the folder: where it is.
fn set_up_extension() -> Result<std::path::PathBuf, String> {
    let dir = crate::store::dir().join("browser-extension");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
    for (name, body) in crate::browser::bridge::FILES {
        std::fs::write(dir.join(name), body).map_err(|e| format!("Couldn't write {name}: {e}"))?;
    }
    let _ = open::that_detached(&dir);
    Ok(dir)
}

fn menu(ui: &mut Ui, c: &Config, live: bool, ctx: &egui::Context) {
    if live && !c.native() && ui.button("Connect").on_hover_text("Starts it now and lists its tools again").clicked() {
        let name = c.name.clone();
        in_background(ctx, &format!("Connecting to {name}"), move || {
            mcp::connect(&name)?;
            let n = mcp::configs().into_iter().find(|c| c.name == name).and_then(|c| mcp::offer(&c)).map_or(0, |o| o.tools.len());
            Ok(format!("{name} answered with {n} tool{}", crate::plural(n)))
        });
        ui.close();
    }
    if mcp::status(&c.name) == Status::Ready && !c.builtin() && ui.button("Disconnect").clicked() {
        mcp::disconnect(&c.name);
        ui.close();
    }
    if let Some(f) = &c.file {
        if ui.button("Open file").clicked() {
            let _ = open::that_detached(f);
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(f.display().to_string());
            ui.close();
        }
    }
    let log = mcp::logs_dir().join(format!("{}.log", c.name));
    if log.is_file() && ui.button("Open log").on_hover_text("What it last wrote to stderr").clicked() {
        let _ = open::that_detached(&log);
        ui.close();
    }
    if c.native() && c.family == "BROWSER" {
        ui.separator();
        if ui
            .button("Set up browser extension")
            .on_hover_text("Writes Oak's extension to a folder and opens it; load it in Brave, Chrome or Edge, and conversations can ask you for one of your tabs (you choose each time)")
            .clicked()
        {
            match set_up_extension() {
                Ok(dir) => {
                    ui.ctx().copy_text(dir.display().to_string());
                    said(
                        format!(
                            "Extension written to {} (path copied). In your browser open brave://extensions (or chrome://extensions, edge://extensions), turn on Developer mode, choose Load unpacked and paste the path.",
                            dir.display()
                        ),
                        false,
                    );
                }
                Err(e) => said(e, true),
            }
            ui.close();
        }
    }
    if c.builtin() && c.name == mcp::blender::NAME {
        ui.separator();
        let installed = !mcp::blender::installed_addons().is_empty();
        if ui.button(if installed { "Install add-on again" } else { "Install add-on" }).clicked() {
            install(ctx);
            ui.close();
        }
        if ui.button("Save add-on file...").on_hover_text("For Blender's Preferences > Add-ons > Install from Disk").clicked() {
            if let Some(to) = rfd::FileDialog::new().set_file_name(format!("{}.py", mcp::blender::ADDON_MODULE)).save_file() {
                match std::fs::write(&to, mcp::blender::ADDON) {
                    Ok(()) => said(format!("Saved {}", to.display()), false),
                    Err(e) => said(format!("Couldn't save {}: {e}", to.display()), true),
                }
            }
            ui.close();
        }
        if let Some(exe) = mcp::blender::find_blender().into_iter().next()
            && ui.button("Open Blender").on_hover_text(exe.display().to_string()).clicked()
        {
            let _ = std::process::Command::new(&exe).spawn();
            ui.close();
        }
        let oak = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "oak".into());
        if ui.button("Copy Claude Code command").on_hover_text("Serves Oak's Blender tools to Claude Code (needs this add-on, or the community one, running in Blender)").clicked() {
            ui.ctx().copy_text(format!("claude mcp add blender -- \"{oak}\" --mcp blender"));
            said("Copied the claude mcp add command".into(), false);
            ui.close();
        }
        if ui.button("Copy Codex config").on_hover_text("For ~/.codex/config.toml").clicked() {
            ui.ctx().copy_text(format!("[mcp_servers.blender]\ncommand = {:?}\nargs = [\"--mcp\", \"blender\"]\n", oak));
            said("Copied the Codex config".into(), false);
            ui.close();
        }
    }
}
