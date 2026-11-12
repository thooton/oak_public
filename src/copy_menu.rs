//! The right-click menu on a chat's read-only text (a transcript's items,
//! a plugin's messages): Copy (the selection), Copy message,
//! Copy link address (the link under the pointer), then the place's own
//! entries (Revert to before this).
//!
//! egui's label selection collapses on any press, the right button's
//! too, so the selection would be gone by the time the menu could copy it.
//! So over the places that have this menu (last pass's, see `show`), the
//! right button never reaches egui: its press is held back, the
//! selection's text is taken then (an injected copy whose clipboard
//! command is dropped), and its release opens the menu. While the menu is
//! open the selection stays, and clicking one of its entries keeps it.

use eframe::egui::{self, Context, Event, Id, LayerId, PointerButton, Pos2, Rect, Ui};

/// A place with the menu: its layer and the part of it on screen.
type Region = (LayerId, Rect);

#[derive(Default)]
pub(crate) struct CopyMenu {
    // input_hook runs before egui enters the input viewport, so it must
    // use RawInput::viewport_id rather than Context::viewport_id.
    views: std::collections::HashMap<egui::ViewportId, MenuState>,
}

impl CopyMenu {
    fn view(&mut self, ctx: &Context) -> &mut MenuState {
        self.views.entry(ctx.viewport_id()).or_default()
    }
}

#[derive(Default)]
struct MenuState {
    /// Places with the menu, registered last pass (where right-clicks are
    /// taken) and this pass.
    last: Vec<Region>,
    /// Hit-testing from this viewport's last pass. Context hit-testing in
    /// input_hook would still hit the parent viewport. A closure holds egui's
    /// Areas snapshot because its concrete type is not publicly exported.
    hit: Option<Box<dyn Fn(Pos2) -> Option<LayerId> + Send + Sync>>,
    this: Vec<Region>,
    /// The right button is down, its press held back from egui.
    held: bool,
    /// This pass carries the copy that takes the selection's text.
    probing: bool,
    /// The output's command count before the label selection's end pass.
    mark: usize,
    /// What was selected when the right button went down.
    selection: Option<String>,
    /// Where the right button was released, over a place with the menu.
    release: Option<Pos2>,
    /// Where the menu opens this pass: the release, a pass later, once the
    /// selection's text is in.
    open_at: Option<Pos2>,
    /// The link under the pointer there (markdown notes it as it draws).
    link: Option<String>,
    /// The label selection as it was then, put back once a click on the
    /// menu has cleared it.
    snapshot: Option<egui::text_selection::LabelSelectionState>,
    /// The menu's rect, shown last pass and this pass.
    menu_last: Option<Rect>,
    menu_this: Option<Rect>,
}

/// Adds the menu's handling to `ctx` (after egui's label selection, whose
/// end of pass it follows).
pub(crate) fn install(ctx: &Context) {
    ctx.add_plugin(CopyMenu::default());
    // The callbacks run after the UI and before the label selection's end
    // of pass, which is where its copy happens.
    ctx.on_end_pass(
        "copy-menu-mark",
        std::sync::Arc::new(|ui: &mut Ui| {
            let n = ui.ctx().output(|o| o.commands.len());
            if let Some(m) = ui.ctx().plugin_opt::<CopyMenu>() {
                m.lock().view(ui.ctx()).mark = n;
            }
        }),
    );
}


impl egui::Plugin for CopyMenu {
    fn debug_name(&self) -> &'static str {
        "copy-menu"
    }

    fn input_hook(&mut self, ctx: &Context, input: &mut egui::RawInput) {
        self.views.entry(input.viewport_id).or_default().input_hook(ctx, input);
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.view(&ctx).on_end_pass(ui);
    }
}

impl MenuState {
    fn over(&self, pos: Pos2) -> bool {
        let top = self.hit.as_ref().and_then(|hit| hit(pos));
        self.last.iter().any(|(layer, r)| r.contains(pos) && top.is_none_or(|t| t == *layer))
    }

    fn input_hook(&mut self, ctx: &Context, input: &mut egui::RawInput) {
        let mut events = Vec::with_capacity(input.events.len());
        for e in input.events.drain(..) {
            match e {
                Event::PointerButton { pos, button: PointerButton::Secondary, pressed: true, .. } if !self.held && self.over(pos) => {
                    self.held = true;
                    self.selection = None;
                    self.link = None;
                    self.snapshot = ctx.plugin_opt::<egui::text_selection::LabelSelectionState>().map(|s| s.lock().clone());
                    self.probing = true;
                    events.push(Event::Copy);
                }
                Event::PointerButton { pos, button: PointerButton::Secondary, pressed: false, .. } if self.held => {
                    self.held = false;
                    if self.over(pos) {
                        self.release = Some(pos);
                    }
                }
                Event::PointerGone if self.held => {
                    self.held = false;
                    events.push(e);
                }
                e => events.push(e),
            }
        }
        input.events = events;
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if std::mem::take(&mut self.probing) {
            // The label selection's copy came after the mark; any other
            // copy the injected event caused (a focused text field's) is
            // dropped too.
            let mark = self.mark;
            ctx.output_mut(|o| {
                let mut i = 0;
                o.commands.retain(|c| {
                    i += 1;
                    match c {
                        egui::OutputCommand::CopyText(s) => {
                            if i > mark && !s.is_empty() {
                                self.selection = Some(s.clone());
                            }
                            false
                        }
                        _ => true,
                    }
                });
            });
            if self.selection.is_none() {
                self.selection = crate::select::selected_text(&ctx);
            }
        }
        // A click on the menu cleared the selection: put it back.
        if let (Some(menu), Some(snap)) = (self.menu_last, &self.snapshot) {
            let on_menu = ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| menu.contains(p)));
            if on_menu && let Some(s) = ctx.plugin_opt::<egui::text_selection::LabelSelectionState>() {
                *s.lock() = snap.clone();
            }
        }
        // Markdown under it notes its link as it draws next pass.
        self.open_at = self.release.take();
        self.link = None;
        if self.open_at.is_some() {
            ctx.request_repaint();
        }
        self.last = std::mem::take(&mut self.this);
        let areas = ctx.memory(|m| m.areas().clone());
        let transforms = ctx.memory(|m| m.layer_ids().collect::<Vec<_>>()).into_iter()
            .filter_map(|layer| ctx.layer_transform_to_global(layer).map(|transform| (layer, transform))).collect();
        self.hit = Some(Box::new(move |pos| areas.layer_id_at(pos, &transforms)));
        self.menu_last = self.menu_this.take();
        if self.menu_last.is_none() && !self.held && self.open_at.is_none() {
            self.snapshot = None;
        }
    }
}

/// A press on the copy menu acts on the selection, rather than ending it.
/// Whole-message selection asks before handling a primary press; egui's
/// ordinary label selection is restored from `snapshot` at the end of pass.
pub(crate) fn selection_menu_pressed(ctx: &Context) -> bool {
    let rect = ctx.plugin_opt::<CopyMenu>().and_then(|m| m.lock().view(ctx).menu_last);
    rect.is_some_and(|r| ctx.input(|i| i.pointer.primary_pressed() && i.pointer.interact_pos().is_some_and(|p| r.contains(p))))
}

/// Where the menu opens this pass, if it does (markdown's links ask, to
/// note the one under it).
pub(crate) fn opening(ctx: &Context) -> Option<Pos2> {
    ctx.plugin_opt::<CopyMenu>().and_then(|m| m.lock().view(ctx).open_at)
}

/// The link under where the menu opens (`opening`): what Copy link address
/// copies.
pub(crate) fn note_link(ctx: &Context, link: String) {
    if let Some(m) = ctx.plugin_opt::<CopyMenu>() {
        m.lock().view(ctx).link = Some(link);
    }
}

/// What the menu for one place offers, taken when it opens.
#[derive(Clone, Default)]
struct Offer {
    selection: Option<String>,
    link: Option<String>,
}

/// The menu on the read-only text in `rect` (in `ui`'s layer), opened by a
/// right-click there: Copy when text is selected, Copy message (with
/// `message`'s text), Copy link address over a link, then `extra`'s
/// entries. `id` is the place's own.
pub(crate) fn show(ui: &mut Ui, id: Id, rect: Rect, message: Option<&dyn Fn() -> String>, extra: Option<&dyn Fn(&mut Ui)>) {
    let ctx = ui.ctx().clone();
    let shown = rect.intersect(ui.clip_rect());
    let plugin = ctx.plugin_opt::<CopyMenu>();
    let open = match &plugin {
        Some(m) => {
            let mut guard = m.lock();
            let m = guard.view(&ctx);
            if shown.is_positive() {
                m.this.push((ui.layer_id(), shown));
            }
            // The innermost place takes it (it registers first).
            match m.open_at {
                Some(p) if shown.contains(p) => {
                    m.open_at = None;
                    Some(Offer { selection: m.selection.clone(), link: m.link.take() })
                }
                _ => None,
            }
        }
        // Without the plugin (some tests), egui's own right-click.
        None => (ui.input(|i| i.pointer.secondary_clicked()) && ui.rect_contains_pointer(shown)).then(Offer::default),
    };
    let offer_id = id.with("copy-menu-offer");
    if let Some(o) = &open {
        if o.selection.is_none() && o.link.is_none() && message.is_none() && extra.is_none() {
            return;
        }
        ui.data_mut(|d| d.insert_temp(offer_id, o.clone()));
    }
    let shown_menu = egui::Popup::new(id.with("copy-menu"), ctx.clone(), rect, ui.layer_id())
        .kind(egui::PopupKind::Menu)
        .layout(egui::Layout::top_down_justified(egui::Align::Min))
        .style(egui::containers::menu::menu_style)
        .open_memory(open.is_some().then_some(egui::SetOpenCommand::Bool(true)))
        .at_pointer_fixed()
        .show(|ui| {
            let o: Offer = ui.data(|d| d.get_temp(offer_id)).unwrap_or_default();
            let mut any = false;
            if let Some(s) = &o.selection
                && ui.button("Copy").clicked()
            {
                ui.ctx().copy_text(s.clone());
            }
            if let Some(m) = message
                && ui.button("Copy message").clicked()
            {
                ui.ctx().copy_text(m().trim().to_string());
            }
            if let Some(l) = &o.link
                && ui.button("Copy link address").clicked()
            {
                ui.ctx().copy_text(l.clone());
            }
            any |= o.selection.is_some() || message.is_some() || o.link.is_some();
            if let Some(extra) = extra {
                if any {
                    ui.separator();
                }
                extra(ui);
            }
        });
    if let (Some(r), Some(m)) = (shown_menu, &plugin) {
        m.lock().view(&ctx).menu_this = Some(r.response.rect);
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::*;

    #[test]
    fn child_input_uses_its_own_regions_before_egui_enters_the_viewport() {
        let ctx = Context::default();
        install(&ctx);
        let child = egui::ViewportId::from_hash_of("copy-menu-child-test");
        let mut time = 0.0;
        let mut frame = |viewport, events| {
            time += 0.05;
            let mut input = egui::RawInput { viewport_id: viewport, screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0))), time: Some(time), events, ..Default::default() };
            input.viewports.entry(viewport).or_default().parent = (viewport != egui::ViewportId::ROOT).then_some(egui::ViewportId::ROOT);
            let mut output = ctx.run_ui(input, |ui| {
                let response = ui.label("Message in this window");
                show(ui, Id::unique(("copy-viewport-test", viewport)), response.rect, Some(&|| "Message in this window".into()), None);
            });
            output.textures_delta.clear();
            output
        };
        for viewport in [egui::ViewportId::ROOT, child] { frame(viewport, vec![]); frame(viewport, vec![]); }
        let plugin = ctx.plugin::<CopyMenu>();
        let point = plugin.lock().views[&child].last[0].1.center();
        frame(child, vec![egui::Event::PointerMoved(point), egui::Event::PointerButton { pos: point, button: PointerButton::Secondary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        {
            let p = plugin.lock();
            assert!(p.views[&child].held, "child's press must be intercepted, even while ctx.viewport_id was ROOT in input_hook");
            assert!(!p.views[&egui::ViewportId::ROOT].held);
            assert!(!p.views[&egui::ViewportId::ROOT].last.is_empty(), "child passes must keep the source's copy regions");
        }
        frame(child, vec![egui::Event::PointerButton { pos: point, button: PointerButton::Secondary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        frame(child, vec![]);
        let output = frame(child, vec![]);
        assert!(crate::app_hover_test::painted(&output).iter().any(|(text, _)| text == "Copy message"));
    }
}
