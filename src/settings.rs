//! The Settings page: theme, font (and a plugin font's download), Home's wallpaper, the
//! interface's zoom (kept in settings.json, see `sync_zoom`), image zoom, touchpad scroll, prompt cache, account rotation, server threads,
//! the connections; a plugin's own settings page (its sections, opened from its row on Home's
//! Plugins tab, never on Settings); and the boxes that add a Codex account, the OpenRouter or
//! NodeClub key, or a local server.

use super::*;
use crate::widgets::{add_field, clip_width, page_card, sign_in_heading};

use crate::composer::{GEAR_ICON, side_box};
use crate::theme::hex;

impl App {
    /// Settings, opened from the gear in a conversation's bottom left
    /// corner or in Home's bottom right one: one box in the conversation's own colors, under a heading
    /// with an X (or Escape) that goes back to where it was opened.
    pub(crate) fn settings_page(&mut self, ui: &mut Ui, t: &Theme) {
        let (card, head, closed) = page_card(ui, t, 780.0, None, "settings-close");
        let p = ui.painter();
        p.text(
            egui::pos2(head.left() + 14.0, head.center().y),
            Align2::LEFT_CENTER,
            GEAR_ICON,
            markdown::icon_font(13.0),
            t.ink3,
        );
        p.text(
            egui::pos2(head.left() + 34.0, head.center().y),
            Align2::LEFT_CENTER,
            "Settings",
            FontId::proportional(13.0),
            t.ink,
        );
        // Esc with a picker open closes only the picker.
        if closed && (self.wallpaper_picker || self.or_picker.is_some()) {
            self.wallpaper_picker = false;
            self.wallpaper_hover = None;
            self.or_picker = None;
        } else if closed {
            self.page = self
                .settings_from
                .take()
                .filter(|p| *p != Page::Settings)
                .unwrap_or(Page::Chats);
        }
        let body = Rect::from_min_max(
            egui::pos2(card.left() + 1.0, head.bottom() + 1.0),
            card.max - vec2(1.0, 1.0),
        );
        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
            ScrollArea::vertical()
                .id_salt("settings-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(16, 16))
                        .show(ui, |ui| {
                            egui::Grid::new("settings-grid")
                                .num_columns(2)
                                .spacing(vec2(28.0, 14.0))
                                .min_col_width(70.0)
                                .show(ui, |ui| self.settings_rows(ui, t));
                        });
                });
        });
    }

    /// Opens the settings page of the plugin `id` (what it registered as
    /// `SettingsSection`s); its X comes back to what was shown.
    pub(crate) fn open_plugin_settings(&mut self, id: &str) {
        if self.page != Page::PluginSettings {
            self.plugin_settings_from = Some(self.page);
        }
        self.plugin_settings = id.to_string();
        self.page = Page::PluginSettings;
    }

    /// A plugin's settings page: Settings' box, under a heading with its
    /// name and an X (or Escape) back to where it was opened (Home's
    /// Plugins tab), its sections down the box (each under its title when
    /// it has several, or one named other than the plugin).
    pub(crate) fn plugin_settings_page(&mut self, ui: &mut Ui, t: &Theme) {
        let id = self.plugin_settings.clone();
        let sections = crate::plugins::parts().settings_of(&id);
        let name = crate::plugins::found()
            .iter()
            .filter_map(|f| f.info.as_ref())
            .find(|i| i.id == id)
            .map(crate::plugins::name_of);
        let (card, head, closed) = page_card(ui, t, 640.0, None, "plugin-settings-close");
        // (Its plugin stopped, or never had any.)
        if closed || sections.is_empty() {
            self.page = self
                .plugin_settings_from
                .take()
                .filter(|p| *p != Page::PluginSettings)
                .unwrap_or(Page::Home);
            return;
        }
        let name = name.unwrap_or_else(|| id.clone());
        let p = ui.painter();
        p.text(
            egui::pos2(head.left() + 14.0, head.center().y),
            Align2::LEFT_CENTER,
            crate::plugins_view::ICON,
            markdown::icon_font(13.0),
            t.ink3,
        );
        let title = clip_width(ui, &name, &FontId::proportional(13.0), head.width() - 80.0);
        p.text(
            egui::pos2(head.left() + 34.0, head.center().y),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(13.0),
            t.ink,
        );
        let body = Rect::from_min_max(
            egui::pos2(card.left() + 1.0, head.bottom() + 1.0),
            card.max - vec2(1.0, 1.0),
        );
        let titled = sections.len() > 1 || sections[0].title() != name;
        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
            ScrollArea::vertical()
                .id_salt(("plugin-settings-scroll", &id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(16, 16))
                        .show(ui, |ui| {
                            ui.set_max_width(560.0);
                            for (i, section) in sections.iter().enumerate() {
                                if i > 0 {
                                    ui.add_space(18.0);
                                }
                                if titled {
                                    caption(ui, &section.title(), t.ink3);
                                    ui.add_space(6.0);
                                }
                                ui.vertical(|ui| section.show(ui, self));
                            }
                        });
                });
        });
    }

    /// The settings grid's rows, top to bottom.
    fn settings_rows(&mut self, ui: &mut Ui, t: &Theme) {
        self.theme_row(ui, t);
        self.font_row(ui, t);
        self.wallpaper_row(ui, t);

        caption(ui, "Zoom", t.ink3);
        zoom_row(ui, self.settings.zoom);
        ui.end_row();

        let tip = "Cache writes: 5 minutes 1.25× input price; 1 hour 2×. Reads: 0.1×. Auto uses conversation timing.";
        if option_row(
            ui,
            t,
            "Prompt cache",
            &mut self.settings.cache,
            [
                ("auto".to_string(), "Auto"),
                ("5m".to_string(), "5 minutes"),
                ("1h".to_string(), "1 hour"),
            ],
            Some(tip),
        ) {
            api::set_cache_mode(&self.settings.cache);
            store::save_settings(&self.settings);
        }
        self.on_off_row(ui, t, "Server threads", |s| &mut s.threads, api::set_threads, "Experimental API. Sends only new messages; the server retains the conversation. Falls back to a full request if needed.");
        self.on_off_row(ui, t, "Keep cache warm", |s| &mut s.keep_alive, api::set_keep_alive, "Claude: rereads an idle conversation's 5-minute cache just before it expires (0.05× input each time), up to 30 minutes. With Auto, the cache is then always 5 minutes.");
        self.on_off_row(ui, t, "Cache diagnostics", |s| &mut s.cache_diagnostics, api::set_diagnostics, "Claude: each request names the previous one, and the API says why a request missed the cache (system, tools or messages changed). Logged in usage.jsonl.");

        if option_row(
            ui,
            t,
            "Title model",
            &mut self.settings.title_model,
            openrouter::TITLE_CHOICES
                .iter()
                .map(|(id, name)| (id.to_string(), *name)),
            Some(
                "Requires an OpenRouter key. Nemotron is free; Gemma costs a fraction of a cent per title. Each is the other's fallback.",
            ),
        ) {
            openrouter::set_title_model(&self.settings.title_model);
            store::save_settings(&self.settings);
        }

        self.quick_handoff_row(ui, t);

        if option_row(
            ui,
            t,
            "OpenRouter routing",
            &mut self.settings.or_routing,
            openrouter::Routing::ALL.map(|r| (r.id().to_string(), r.label())),
            Some(
                "Which provider serves an OpenRouter model. Balanced: cost times the square root of time. \
             Cheapest: cost per request, counting cache reads (most of an agent's input). Fastest: first token plus reply time. \
             OpenRouter: its own routing within OAK's price ceiling. A conversation stays with its provider while it is within 1.3× of the best, to keep its cache.",
            ),
        ) {
            openrouter::set_routing(&self.settings.or_routing);
            store::save_settings(&self.settings);
        }
        self.or_models_row(ui, t);

        if option_row(
            ui,
            t,
            "Local summaries",
            &mut self.settings.local_summarizer,
            rolling::Summarizer::ALL.map(|s| (s.id().to_string(), s.label())),
            Some(
                "Local summaries stay on your server and pause the session. DeepSeek sends context to OpenRouter; \
             Haiku to Claude. Requires the corresponding account; falls back to local.",
            ),
        ) {
            rolling::set_summarizer(rolling::Summarizer::from_id(
                &self.settings.local_summarizer,
            ));
            store::save_settings(&self.settings);
        }

        // Each connection's state; they are added and removed on Home.
        caption(ui, "Connections", t.ink3);
        // Two lines: the providers outgrow one line of the dialog.
        ui.vertical(|ui| {
            for line in connections().chunks(4) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 18.0;
                    for (name, state, tip) in line {
                        status_dot(ui, t, name, *state).on_hover_text(tip);
                    }
                });
            }
        });
        ui.end_row();

        self.on_off_row(ui, t, "Account rotation", |s| &mut s.rotation, api::set_rotation, "Switches to the account with the most remaining usage at a limit. Off: waits for the selected account to reset.");
        let tip = "Hard limit on a session's notes, all together. At it, the model must shorten or delete notes before adding or lengthening one; \
                   notes already over it are kept. Running sessions are told of a change in a message.";
        if option_row(
            ui,
            t,
            "Notes limit",
            &mut self.settings.notes_limit,
            crate::agent::NOTES_LIMITS,
            Some(tip),
        ) {
            crate::agent::set_notes_limit(self.settings.notes_limit);
            store::save_settings(&self.settings);
        }
        self.on_off_row(ui, t, "Notes reminder", |s| &mut s.notes_alert, crate::agent::set_notes_alert, "Model reminder to prune notes past 85% of the notes limit, or once they are over it. Off: the model is never told to shrink its notes (NOTE still refuses to grow them past the limit).");

        caption(ui, "Scratchpads", t.ink3);
        ui.horizontal(|ui| self.scratch_row(ui, t));
        ui.end_row();

        if option_row(
            ui,
            t,
            "Image zoom",
            &mut self.settings.click_zoom,
            [(false, "Scroll"), (true, "Click")],
            Some("Scroll: wheel to zoom, right-drag to pan. Click: toggle zoom, drag to pan."),
        ) {
            store::save_settings(&self.settings);
        }

        if cfg!(all(unix, not(target_os = "macos"))) {
            caption(ui, "Touchpad scroll", t.ink3);
            let r = ui
                .add(
                    egui::Slider::new(&mut self.settings.touchpad_speed, 1.0..=8.0)
                        .step_by(0.5)
                        .suffix("×"),
                )
                .on_hover_text("Touchpad sensitivity; mouse wheel unchanged.");
            // Saved once a drag ends, not on every frame of it.
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                store::save_settings(&self.settings);
            }
            ui.end_row();
        }
    }

    /// An Off/On row for the switch `field`, which `apply` puts in force
    /// (and which is saved) when flipped.
    fn on_off_row(
        &mut self,
        ui: &mut Ui,
        t: &Theme,
        label: &str,
        field: fn(&mut Settings) -> &mut bool,
        apply: fn(bool),
        tip: &str,
    ) {
        if option_row(ui, t, label, field(&mut self.settings), OFF_ON, Some(tip)) {
            apply(*field(&mut self.settings));
            store::save_settings(&self.settings);
        }
    }

    /// The Theme row: the built-in themes and those plugins add.
    fn theme_row(&mut self, ui: &mut Ui, t: &Theme) {
        let themes = crate::theme::themes();
        // One whose plugin isn't loaded stays the setting; Oak's shows.
        let now = if themes.iter().any(|(id, _)| *id == self.settings.theme) {
            self.settings.theme.clone()
        } else {
            crate::theme::OAK.to_string()
        };
        if themes.len() < 2 {
            return;
        }
        caption(ui, "Theme", t.ink3);
        ui.horizontal_wrapped(|ui| {
            for (id, name) in &themes {
                if ui.selectable_label(now == *id, name).clicked() && now != *id {
                    self.settings.theme = id.clone();
                    apply_theme(ui.ctx(), &crate::theme::theme(id));
                    store::save_settings(&self.settings);
                }
            }
        });
        ui.end_row();
    }

    /// The Wallpaper row: Home's background, Oak's forest or a picture from
    /// the wallpapers folder. The button (the one picked, small) opens the
    /// grid to pick another from.
    fn wallpaper_row(&mut self, ui: &mut Ui, t: &Theme) {
        let list = crate::wallpaper::list();
        let now = list.iter().find(|w| w.file == self.settings.wallpaper);
        caption(ui, "Wallpaper", t.ink3);
        let name = now.map_or("Forest", |w| &w.name).to_string();
        let tex = match now {
            Some(w) => crate::wallpaper::thumbnail(ui.ctx(), w),
            None => Some(crate::home::picture(
                ui.ctx(),
                &mut self.pictures,
                "forest-home",
            )),
        };
        let (rect, resp) = ui.allocate_exact_size(vec2(260.0, 40.0), Sense::click());
        let p = ui.painter();
        p.rect_filled(
            rect,
            4.0,
            if resp.hovered() {
                t.tool_hover
            } else {
                t.panel2
            },
        );
        p.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0, t.line),
            egui::StrokeKind::Inside,
        );
        let thumb = Rect::from_min_size(rect.min + vec2(4.0, 4.0), vec2(57.0, 32.0));
        match &tex {
            Some(tex) => {
                crate::home::paint_cover(p, thumb, tex, Color32::WHITE);
            }
            None => {
                p.rect_filled(thumb, 2.0, t.bg);
            }
        }
        p.text(
            egui::pos2(thumb.right() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            FontId::proportional(13.0),
            t.ink,
        );
        p.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            "Change",
            FontId::proportional(12.0),
            t.ink3,
        );
        if resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            self.wallpaper_picker = true;
        }
        ui.end_row();
        if self.wallpaper_picker {
            self.wallpaper_picker(ui.ctx(), t, &list);
        }
    }

    /// The wallpapers as a grid, three to a row: the forest, then the
    /// folder's; the one under the pointer plays. A click picks one and
    /// closes it, as do Esc, the X and a click outside.
    fn wallpaper_picker(
        &mut self,
        ctx: &egui::Context,
        t: &Theme,
        list: &[crate::wallpaper::Wallpaper],
    ) {
        const TILE: egui::Vec2 = vec2(224.0, 126.0);
        const GAP: f32 = 12.0;
        let mut pick = None;
        let mut close = false;
        let mut hovered = None;
        let modal = crate::widgets::dialog("wallpaper-picker", t).show(ctx, |ui| {
            ui.set_width(TILE.x * 3.0 + GAP * 2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Wallpaper").size(16.0).color(t.ink));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let (x, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                    crate::widgets::draw_x(ui.painter(), x.shrink(7.0), if resp.hovered() { t.ink } else { t.ink3 });
                    close |= resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                    let folder = crate::wallpaper::folder();
                    let tip = format!("PNG, JPEG, GIF or WebP files put in\n{}\nshow here. Animated GIF and WebP play.", folder.display());
                    if ui.button("Open folder").on_hover_text(tip).clicked() {
                        let _ = std::fs::create_dir_all(&folder);
                        let _ = open::that_detached(&folder);
                    }
                });
            });
            ui.add_space(8.0);
            let max_h = (ctx.content_rect().height() * 0.75 - 60.0).max(TILE.y + 30.0);
            // (The forest's tile first.)
            let rows = (list.len() + 1).div_ceil(3) as f32;
            let full = rows * (TILE.y + 30.0);
            ScrollArea::vertical().id_salt("wallpaper-grid").max_height(max_h).min_scrolled_height(full.min(max_h)).auto_shrink([false, true]).show(ui, |ui| {
                let tiles: Vec<Option<&crate::wallpaper::Wallpaper>> = std::iter::once(None).chain(list.iter().map(Some)).collect();
                for row in tiles.chunks(3) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        for w in row {
                            let file = w.map_or(String::new(), |w| w.file.clone());
                            let (rect, resp) = ui.allocate_exact_size(TILE + vec2(0.0, 24.0), Sense::click());
                            let pic = Rect::from_min_size(rect.min, TILE);
                            let hover = resp.hovered();
                            if hover && w.is_some() {
                                hovered = Some(file.clone());
                            }
                            let playing = self.wallpaper_hover.as_mut().filter(|p| hover && p.file == file).and_then(|p| p.frame(ui.ctx())).cloned();
                            let tex = playing.or_else(|| match w {
                                Some(w) => crate::wallpaper::thumbnail(ui.ctx(), w),
                                None => Some(crate::home::picture(ui.ctx(), &mut self.pictures, "forest-home")),
                            });
                            let p = ui.painter();
                            match &tex {
                                Some(tex) => {
                                    crate::home::paint_cover(p, pic, tex, Color32::WHITE);
                                }
                                None => {
                                    p.rect_filled(pic, 0.0, t.bg);
                                }
                            }
                            let chosen = self.settings.wallpaper == file;
                            let stroke = if chosen { Stroke::new(2.0, t.accent) } else if hover { Stroke::new(1.0, t.ink3) } else { Stroke::new(1.0, t.line) };
                            p.rect_stroke(pic, 0.0, stroke, egui::StrokeKind::Inside);
                            let name = w.map_or("Forest", |w| &w.name);
                            let label = egui::pos2(pic.left() + 2.0, pic.bottom() + 12.0);
                            p.text(label, Align2::LEFT_CENTER, name, FontId::proportional(12.5), if chosen || hover { t.ink } else { t.ink2 });
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                pick = Some(file);
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
            });
        });
        // The one under the pointer plays; the player goes with the pointer.
        match hovered {
            Some(file) if self.wallpaper_hover.as_ref().is_none_or(|p| p.file != file) => {
                self.wallpaper_hover = Some(crate::wallpaper::Player::new(&file))
            }
            None => self.wallpaper_hover = None,
            _ => {}
        }
        if let Some(file) = pick {
            if file != self.settings.wallpaper {
                self.settings.wallpaper = file;
                store::save_settings(&self.settings);
            }
            close = true;
        }
        if close || modal.should_close() {
            self.wallpaper_picker = false;
            self.wallpaper_hover = None;
        }
    }

    /// Ordered, tool-free model attempts for handing off Quick's transcript.
    fn quick_handoff_row(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "Quick handoff models", t.ink3);
        let mut change = None;
        let mut add = None;
        let list = &self.settings.quick_handoff_models;
        ui.vertical(|ui| {
            let about = "When Quick runs out of usage, Oak sends conversation context to these models in order to write a handoff for the next Quick account.";
            ui.add(egui::Label::new(RichText::new(about).small().color(t.ink2)).wrap());
            if list.is_empty() {
                caption(ui, "Automatic: one available model per provider", t.ink3);
            }
            for (i, writer) in list.iter().enumerate() {
                ui.push_id(("quick-handoff", i), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}. {} ({})", i + 1, self.model_name(&writer.model), writer.provider.name()));
                        if ui.add_enabled(i > 0, egui::Button::new("↑")).on_hover_text("Try earlier").clicked() {
                            change = Some((i, Some(i - 1)));
                        }
                        if ui.add_enabled(i + 1 < list.len(), egui::Button::new("↓")).on_hover_text("Try later").clicked() {
                            change = Some((i, Some(i + 1)));
                        }
                        if ui.button("Remove").clicked() { change = Some((i, None)); }
                    });
                });
            }
            let button = ui.button("Add model…");
            egui::Popup::menu(&button).width(180.0).show(|ui| {
                let models = api::known_models();
                let mut any = false;
                for provider in api::Provider::ALL.into_iter().filter(|p| crate::quick::recovery::can_write(*p)) {
                    let group: Vec<_> = models.iter().filter(|m| m.provider == provider
                        && !list.iter().any(|f| f.provider == provider && f.model == m.id)).collect();
                    if group.is_empty() { continue; }
                    any = true;
                    ui.menu_button(provider.name(), |ui| {
                        for m in group {
                            if ui.button(&m.name).clicked() {
                                add = Some(store::Fallback { provider, model: m.id.clone() });
                                ui.close();
                            }
                        }
                    });
                }
                if !any { caption(ui, "No other models available", t.ink3); }
            });
        });
        if change.is_some() || add.is_some() {
            let list = &mut self.settings.quick_handoff_models;
            match change {
                Some((i, Some(j))) => list.swap(i, j),
                Some((i, None)) => { list.remove(i); }
                None => {}
            }
            list.extend(add);
            store::save_settings(&self.settings);
        }
        ui.end_row();
    }

    /// The OpenRouter models row: how many are built in (named on hover),
    /// the ones added (a click removes one), and Add, which opens the
    /// picker of OpenRouter's list.
    fn or_models_row(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "OpenRouter models", t.ink3);
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            let built: Vec<&str> = openrouter::MODELS.iter().map(|m| m.name).collect();
            ui.label(RichText::new(format!("{} built in", built.len())).color(t.ink2)).on_hover_text(built.join("\n"));
            for (i, a) in self.settings.or_models.iter().enumerate() {
                let name = if a.name.is_empty() { &a.id } else { &a.name };
                if ui.button(format!("{name}  ×")).on_hover_text(format!("{}\nClick to remove", a.id)).clicked() {
                    remove = Some(i);
                }
            }
            if ui.button("Add…").on_hover_text("Pick from OpenRouter's models that call tools. Added ones show in the model picker with an OpenRouter key.").clicked() {
                self.or_picker = Some(String::new());
            }
        });
        ui.end_row();
        if let Some(i) = remove {
            self.settings.or_models.remove(i);
            self.or_models_changed();
        }
        if self.or_picker.is_some() {
            self.or_model_picker(ui.ctx(), t);
        }
    }

    fn or_models_changed(&mut self) {
        openrouter::set_added(&self.settings.or_models);
        store::save_settings(&self.settings);
    }

    /// OpenRouter's models that call tools, searchable by name or id: a
    /// click adds one to the picker, or removes one added. Built-in ones
    /// show as such. Closed by Esc, the X or a click outside.
    fn or_model_picker(&mut self, ctx: &egui::Context, t: &Theme) {
        const ROW: f32 = 40.0;
        let Some(mut query) = self.or_picker.take() else {
            return;
        };
        let cat = openrouter::catalog(ctx);
        let words: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let shown: Vec<&openrouter::Listed> = cat
            .list
            .iter()
            .filter(|l| {
                let hay = format!("{} {}", l.added.name, l.added.id).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        let mut close = false;
        let mut toggle = None;
        let modal = crate::widgets::dialog("or-model-picker", t).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("OpenRouter models").size(16.0).color(t.ink));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let (x, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                    crate::widgets::draw_x(
                        ui.painter(),
                        x.shrink(7.0),
                        if resp.hovered() { t.ink } else { t.ink3 },
                    );
                    close |= resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked();
                    if cat.loading {
                        ui.add(egui::Spinner::new().size(12.0).color(t.ink3));
                    }
                });
            });
            ui.add_space(8.0);
            let edit = TextEdit::singleline(&mut query)
                .id(Id::unique("or-model-search"))
                .hint_text("Search")
                .desired_width(f32::INFINITY);
            let field = crate::text_menu::add(ui, edit);
            if !field.has_focus() && ui.memory(|m| m.focused().is_none()) {
                field.request_focus();
            }
            ui.add_space(6.0);
            if let Some(e) = &cat.error {
                crate::widgets::error_note(
                    ui,
                    t,
                    &format!("Couldn't load OpenRouter's models: {e}"),
                );
            } else if cat.list.is_empty() {
                caption(ui, "Loading…", t.ink3);
            } else if shown.is_empty() {
                caption(ui, "No matches", t.ink3);
            }
            let max_h = (ctx.content_rect().height() * 0.7 - 90.0).max(ROW * 4.0);
            let full = shown.len() as f32 * ROW;
            ScrollArea::vertical()
                .id_salt("or-model-list")
                .max_height(max_h)
                .min_scrolled_height(full.min(max_h))
                .auto_shrink([false, true])
                .show_rows(ui, ROW, shown.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for l in &shown[range] {
                        let a = &l.added;
                        let built = openrouter::MODELS.iter().any(|m| m.id == a.id);
                        let added = self.settings.or_models.iter().any(|m| m.id == a.id);
                        let (rect, resp) = ui.allocate_exact_size(
                            vec2(ui.available_width(), ROW),
                            if built {
                                Sense::hover()
                            } else {
                                Sense::click()
                            },
                        );
                        let hover = resp.hovered() && !built;
                        let p = ui.painter();
                        if hover {
                            p.rect_filled(rect, 3.0, t.tool_hover);
                        }
                        let right = rect.right() - 10.0;
                        let (status, color) = match (built, added) {
                            (true, _) => ("Built in", t.ink3),
                            (_, true) => ("Added", t.accent),
                            _ => ("", t.ink3),
                        };
                        p.text(
                            egui::pos2(right, rect.center().y),
                            Align2::RIGHT_CENTER,
                            status,
                            FontId::proportional(12.0),
                            color,
                        );
                        let price = if a.free {
                            "free".to_string()
                        } else {
                            format!("${} / ${} per M", per_m(l.price.0), per_m(l.price.1))
                        };
                        let facts = format!(
                            "{price}, {}k context{}",
                            a.context / 1000,
                            if a.reasoning { "" } else { ", no thinking" }
                        );
                        let facts_x = right - 70.0;
                        p.text(
                            egui::pos2(facts_x, rect.center().y),
                            Align2::RIGHT_CENTER,
                            facts,
                            FontId::proportional(11.5),
                            t.ink3,
                        );
                        let w = (facts_x - rect.left() - 190.0).max(120.0);
                        let name = clip_width(ui, &a.name, &FontId::proportional(13.0), w);
                        let id = clip_width(ui, &a.id, &FontId::proportional(11.0), w);
                        let p = ui.painter();
                        p.text(
                            egui::pos2(rect.left() + 8.0, rect.top() + 13.0),
                            Align2::LEFT_CENTER,
                            name,
                            FontId::proportional(13.0),
                            if added || hover { t.ink } else { t.ink2 },
                        );
                        p.text(
                            egui::pos2(rect.left() + 8.0, rect.top() + 29.0),
                            Align2::LEFT_CENTER,
                            id,
                            FontId::proportional(11.0),
                            t.ink3,
                        );
                        if !built
                            && resp
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(if added { "Remove" } else { "Add" })
                                .clicked()
                        {
                            toggle = Some(a.clone());
                        }
                    }
                });
        });
        if let Some(a) = toggle {
            match self.settings.or_models.iter().position(|m| m.id == a.id) {
                Some(i) => drop(self.settings.or_models.remove(i)),
                None => self.settings.or_models.push(a),
            }
            self.or_models_changed();
        }
        if !close && !modal.should_close() {
            self.or_picker = Some(query);
        }
    }

    /// The Font row: IBM Plex Sans, and those plugins add (one is fetched
    /// when picked the first time, with the download's progress or error).
    fn font_row(&mut self, ui: &mut Ui, t: &Theme) {
        let faces = markdown::faces();
        if faces.len() < 2 {
            return;
        }
        let job = self.font_job.lock().unwrap().clone();
        let now = if faces.iter().any(|(id, _)| *id == self.settings.font) {
            self.settings.font.clone()
        } else {
            markdown::PLEX.to_string()
        };
        caption(ui, "Font", t.ink3);
        ui.horizontal(|ui| {
            for (id, name) in &faces {
                if ui.selectable_label(now == *id, name).clicked() && now != *id {
                    self.settings.font = id.clone();
                    store::save_settings(&self.settings);
                    if !markdown::install_face(ui.ctx(), id) {
                        download_font(ui.ctx(), &self.font_job, id);
                    }
                }
            }
            if job.busy {
                ui.add(egui::Spinner::new().size(12.0).color(t.ink3));
                caption(ui, "downloading the font…", t.ink3);
            } else if let Some(e) = &job.error {
                caption(ui, e, t.error);
            }
        });
        ui.end_row();
    }

    /// The Scratchpads row: their size, and how long idle before one is
    /// cleared.
    fn scratch_row(&mut self, ui: &mut Ui, t: &Theme) {
        let scan = crate::scratch::scan(Duration::from_secs(15), ui.ctx());
        let size = match scan.at {
            Some(_) => {
                let n = scan.pads.len();
                format!(
                    "{} in {n} folder{}",
                    crate::scratch::fmt_bytes(scan.bytes()),
                    crate::plural(n)
                )
            }
            None => "measuring…".to_string(),
        };
        ui.label(RichText::new(size).color(t.ink2)).on_hover_text(
            "Every session's .oak/tmp/tempN folder, in the working folders of all conversations.",
        );
        ui.add_space(12.0);
        caption(ui, "Clear when idle for", t.ink3);
        let before = self.settings.scratch_clear;
        let shown = crate::scratch::AGES
            .iter()
            .find(|a| a.0 == before)
            .map_or("Never", |a| a.1);
        egui::ComboBox::from_id_salt("scratch-clear").selected_text(shown).show_ui(ui, |ui| {
            for (age, label) in crate::scratch::AGES {
                ui.selectable_value(&mut self.settings.scratch_clear, age, label);
            }
        })
        .response
        .on_hover_text("Empties a session's scratchpad once neither the session nor its files changed for this long. Sessions at work or with running tasks are skipped.");
        if before != self.settings.scratch_clear {
            crate::scratch::set_auto(self.settings.scratch_clear);
            store::save_settings(&self.settings);
        }
    }

    /// Signing in to Codex from Home: the box for the address the browser
    /// ends at (when nothing listened for it), the size of an account box.
    /// True once signed in.
    pub(crate) fn codex_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        let Some(login) = self.codex_login.take() else {
            return false;
        };
        let (mut done, mut keep) = (false, true);
        match codex::login_result(&login) {
            Some(Ok(())) => done = true,
            Some(Err(e)) => self.codex_error = Some(e),
            // The browser comes back on another thread: look again soon.
            None => ui.ctx().request_repaint_after(Duration::from_millis(300)),
        }
        if !done {
            let (mut add, mut cancel) = (false, false);
            side_box(ui, t, w, |ui| {
                sign_in_heading(ui, t, "Add Codex account", &login.url);
                let edit = TextEdit::singleline(&mut self.codex_paste)
                    .id(Id::unique("add-codex-address"))
                    .hint_text("Callback URL");
                (add, cancel) = add_field(ui, t, edit, None, self.codex_error.as_ref(), |ui| {
                    if login.listening {
                        caption(ui, crate::widgets::LINK_COPIED, t.ink3);
                    } else {
                        ui.label(RichText::new("Paste the callback URL after sign-in.").small().color(t.ink3))
                            .on_hover_text("localhost:1455 is unavailable. Copy the callback URL even if the page fails to load.");
                    }
                });
            });
            if cancel {
                keep = false;
            } else if add && !self.codex_paste.trim().is_empty() {
                match codex::finish_login(&login, &self.codex_paste) {
                    Ok(()) => done = true,
                    Err(e) => self.codex_error = Some(e),
                }
            }
        }
        if done || !keep {
            self.codex_paste.clear();
            self.codex_error = None;
        } else {
            self.codex_login = Some(login);
        }
        done
    }

    /// Signing in to Antigravity from Home: listens on localhost:51121, with a
    /// field for pasting the callback URL if the browser cannot reach it.
    /// True once signed in.
    pub(crate) fn antigravity_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        let Some(login) = self.antigravity_login.take() else {
            return false;
        };
        let (mut done, mut keep) = (false, true);
        match antigravity::login_result(&login) {
            Some(Ok(())) => done = true,
            Some(Err(e)) => self.antigravity_error = Some(e),
            None => ui.ctx().request_repaint_after(Duration::from_millis(300)),
        }
        if !done {
            let (mut add, mut cancel) = (false, false);
            side_box(ui, t, w, |ui| {
                sign_in_heading(ui, t, "Add Antigravity account", &login.url);
                let edit = TextEdit::singleline(&mut self.antigravity_paste)
                    .id(Id::unique("add-antigravity-address"))
                    .hint_text("Callback URL");
                (add, cancel) = add_field(
                    ui,
                    t,
                    edit,
                    None,
                    self.antigravity_error.as_ref(),
                    |ui| {
                        if login.listening {
                            caption(ui, crate::widgets::LINK_COPIED, t.ink3);
                        } else {
                            ui.label(RichText::new("Paste the callback URL after sign-in.").small().color(t.ink3))
                            .on_hover_text("localhost:51121 is unavailable. Copy the callback URL even if the page fails to load.");
                        }
                    },
                );
            });
            if cancel {
                keep = false;
            } else if add && !self.antigravity_paste.trim().is_empty() {
                match antigravity::finish_login(&login, &self.antigravity_paste) {
                    Ok(()) => done = true,
                    Err(e) => self.antigravity_error = Some(e),
                }
            }
        }
        if done || !keep {
            self.antigravity_paste.clear();
            self.antigravity_error = None;
        } else {
            self.antigravity_login = Some(login);
        }
        done
    }

    /// Adding the user's own llama.cpp server from Home, or changing the
    /// one added (`LocalForm::edit`): its address (the launcher's
    /// `--port`), and optionally a name for its model and a cap on its
    /// context (empty: what the server says). True once saved.
    pub(crate) fn local_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        let (mut add, mut cancel, mut enter) = (false, false, false);
        let f = &mut self.local_form;
        let known = crate::local::server().filter(|_| f.editing);
        side_box(ui, t, w, |ui| {
            caption(ui, if f.editing { "Edit local llama.cpp server" } else { "Add local llama.cpp server" }, t.ink2);
            fn small(edit: TextEdit<'_>) -> TextEdit<'_> {
                edit.font(egui::TextStyle::Small).margin(egui::Margin::symmetric(4, 2)).desired_width(f32::INFINITY)
            }
            let url = crate::text_menu::add(ui, small(TextEdit::singleline(&mut f.url).id(Id::unique("add-local-url")).hint_text(crate::local::DEFAULT_URL)));
            enter |= crate::widgets::left_with(ui, &url, egui::Key::Enter);
            // The hints say what is used when the field is left empty.
            let auto_name = known.as_ref().map_or("Name (from the server)".to_string(), |s| {
                format!("Name ({}, from the server)", crate::local::Server { name: String::new(), ..s.clone() }.display_name())
            });
            let name = crate::text_menu::add(ui, small(TextEdit::singleline(&mut f.name).id(Id::unique("add-local-name")).hint_text(auto_name)));
            enter |= crate::widgets::left_with(ui, &name, egui::Key::Enter);
            let auto_ctx = match known.as_ref().map(|s| s.context) {
                Some(n) if n > 0 => format!("Context ({}, the server's)", crate::local::context_text(n)),
                _ => "Context (the server's, e.g. 64k)".to_string(),
            };
            let edit = TextEdit::singleline(&mut f.context).id(Id::unique("add-local-context")).hint_text(auto_ctx);
            let label = if f.editing { "Save" } else { "Add" };
            (add, cancel) = crate::widgets::labeled_add_field(ui, t, edit, None, f.error.as_ref(), |ui| {
                caption(ui, "Empty fields: what the server says when it runs.", t.ink3);
            }, label);
        });
        let add = (add || enter) && !cancel;
        let mut done = false;
        if add {
            match crate::local::parse_context(&f.context) {
                Ok(limit) => {
                    crate::local::add(&f.url, &f.name, limit);
                    done = true;
                }
                Err(e) => f.error = Some(e),
            }
        }
        if cancel || done {
            self.adding_local = false;
            self.local_form = LocalForm::default();
        }
        done
    }

    /// Adding the OpenRouter key from Home, in a box the size of an
    /// account box. True once added.
    pub(crate) fn openrouter_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        key_add_box(
            ui,
            t,
            w,
            ("Add OpenRouter key", "add-openrouter-key", "sk-or-v1-…"),
            || "openrouter.ai/keys".to_string(),
            &mut self.adding_openrouter,
            openrouter::set_key,
        )
    }

    /// Adding the NodeClub key from Home, as the OpenRouter one: checked
    /// with NodeClub, then kept on the vault when connected to
    /// one (for every OAK), else on this machine. True once added.
    pub(crate) fn nodeclub_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        key_add_box(
            ui,
            t,
            w,
            ("Add NodeClub key", "add-nodeclub-key", "sk-…"),
            || {
                let to = if api::signing_in_remotely() {
                    format!("kept on {}", api::vault_name())
                } else {
                    "kept on this machine".to_string()
                };
                format!("api.nodeclub.ai, {to}")
            },
            &mut self.adding_nodeclub,
            crate::nodeclub::set_key,
        )
    }

    /// Adding a Muse Code key (a Meta Model API key) from Home, as the
    /// NodeClub one: checked with the API (its model list), then kept on
    /// the vault when connected to one (for every OAK), else on this
    /// machine. True once added.
    pub(crate) fn musecode_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        key_add_box(
            ui,
            t,
            w,
            ("Add Muse Code key", "add-musecode-key", "LLM_…"),
            || {
                let to = if api::signing_in_remotely() {
                    format!("kept on {}", api::vault_name())
                } else {
                    "kept on this machine".to_string()
                };
                format!("A Meta Model API key from dev.meta.ai, {to}")
            },
            &mut self.adding_musecode,
            crate::musecode::set_key,
        )
    }

    /// Adding a Refuseless key from Home, as the NodeClub one: checked with
    /// Refuseless (its credits), then kept on the vault when connected to
    /// one (for every OAK), else on this machine. True once added.
    pub(crate) fn refuseless_add_box(&mut self, ui: &mut Ui, t: &Theme, w: f32) -> bool {
        key_add_box(
            ui,
            t,
            w,
            ("Add Refuseless key", "add-refuseless-key", "abl_…"),
            || {
                let to = if api::signing_in_remotely() {
                    format!("kept on {}", api::vault_name())
                } else {
                    "kept on this machine".to_string()
                };
                format!("refuseless.com, {to}")
            },
            &mut self.adding_refuseless,
            crate::refuseless::set_key,
        )
    }
}

/// A key being added from Home: its box is open, the key typed, and why
/// saving it failed.
#[derive(Default)]
pub(crate) struct KeyForm {
    pub(crate) open: bool,
    key: String,
    error: Option<String>,
}

/// The local server's box on Home: what is typed, whether it changes the
/// server added rather than adding one, and why saving failed.
#[derive(Default)]
pub(crate) struct LocalForm {
    url: String,
    name: String,
    context: String,
    editing: bool,
    error: Option<String>,
}

impl LocalForm {
    /// The box filled in with the server added, to change it.
    pub(crate) fn edit(s: &crate::local::Server) -> Self {
        let context = if s.context_limit > 0 { crate::local::context_text(s.context_limit) } else { String::new() };
        LocalForm { url: s.url.clone(), name: s.name.clone(), context, editing: true, error: None }
    }
}

/// The OpenRouter, NodeClub, Refuseless and Muse Code key boxes from Home: a caption, the key in a
/// password field, a line (its error, or `note`), and Add and Cancel.
/// The key goes to `set_key`; on success, the form closes. True once added.
fn key_add_box(
    ui: &mut Ui,
    t: &Theme,
    w: f32,
    (title, id, hint): (&str, &str, &str),
    note: impl Fn() -> String,
    form: &mut KeyForm,
    set_key: fn(&str) -> Result<(), String>,
) -> bool {
    let KeyForm {
        open: adding,
        key,
        error,
    } = form;
    let (mut add, mut cancel) = (false, false);
    side_box(ui, t, w, |ui| {
        caption(ui, title, t.ink2);
        let edit = TextEdit::singleline(key).id(Id::unique(id)).hint_text(hint);
        (add, cancel) = add_field(ui, t, edit, Some(id), error.as_ref(), |ui| {
            caption(ui, &note(), t.ink3);
        });
    });
    let mut done = false;
    if cancel {
        *adding = false;
    } else if add && !key.trim().is_empty() {
        match set_key(key) {
            Ok(()) => {
                done = true;
                *adding = false;
            }
            Err(e) => *error = Some(e),
        }
    }
    if !*adding {
        ui.data_mut(|d| d.remove_temp::<bool>(crate::widgets::secret_key(id)));
        key.clear();
        *error = None;
    }
    done
}

/// Fetching a plugin's font.
#[derive(Clone, Default)]
pub(crate) struct FontJob {
    busy: bool,
    error: Option<String>,
    /// Fetched: install the chosen font again.
    pub(crate) reinstall: bool,
}

/// Fetches the plugin font `face` in the background; the next frame
/// installs it if it is still the chosen font.
pub(crate) fn download_font(ctx: &egui::Context, job: &Arc<Mutex<FontJob>>, face: &str) {
    let Some(font) = markdown::plugin_face(face) else {
        return;
    };
    let mut j = job.lock().unwrap();
    if j.busy {
        return;
    }
    j.busy = true;
    j.error = None;
    drop(j);
    let (job, ctx) = (job.clone(), ctx.clone());
    std::thread::spawn(move || {
        let r = font.fetch();
        let mut j = job.lock().unwrap();
        j.busy = false;
        j.error = r.err();
        j.reinstall = j.error.is_none();
        ctx.request_repaint();
    });
}

/// How a connection is: working, failing, or not set up.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Conn {
    Ok,
    Failing,
    Off,
}

/// The connections Settings lists: Claude, Codex, Antigravity, OpenRouter, Muse Code,
/// NodeClub, Freebuff, Refuseless and the local llama.cpp server, how each is, and what its dot's
/// hover says.
fn connections() -> [(&'static str, Conn, String); 9] {
    let kept = |remote: bool| {
        if remote {
            " (shared)"
        } else if api::vault_on() {
            " (on this machine)"
        } else {
            ""
        }
    };
    let accounts = api::accounts();
    let claude = if accounts.is_empty() {
        (Conn::Off, "No Claude account: add one on Home.".to_string())
    } else if let Some(e) = accounts
        .iter()
        .all(|a| a.error.is_some() && a.limits.is_empty())
        .then(|| accounts[0].error.clone().unwrap_or_default())
    {
        (Conn::Failing, e)
    } else {
        let n = accounts.len();
        (Conn::Ok, format!("{n} account{}", crate::plural(n)))
    };
    let codex = match codex::account() {
        None => (Conn::Off, "No Codex account: add one on Home.".to_string()),
        Some(a) => match a.error.filter(|_| a.limits.is_empty()) {
            Some(e) => (Conn::Failing, e),
            None => {
                let who: Vec<&str> = [a.email.as_str(), a.plan.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                (
                    Conn::Ok,
                    format!(
                        "{}{}",
                        if who.is_empty() {
                            "Signed in".to_string()
                        } else {
                            who.join(", ")
                        },
                        kept(codex::remote())
                    ),
                )
            }
        },
    };
    let agy = match antigravity::account() {
        None => (
            Conn::Off,
            "No Antigravity account: add one on Home.".to_string(),
        ),
        Some(a) => match a.error.filter(|_| a.limits.is_empty()) {
            Some(e) => (Conn::Failing, e),
            None => {
                let who: Vec<&str> = [a.email.as_str(), a.plan.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                (
                    Conn::Ok,
                    format!(
                        "{}{}",
                        if who.is_empty() {
                            "Signed in".to_string()
                        } else {
                            who.join(", ")
                        },
                        kept(antigravity::remote())
                    ),
                )
            }
        },
    };
    let router = match openrouter::account() {
        None => (Conn::Off, "No OpenRouter key: add one on Home.".to_string()),
        Some(a) => match a.error {
            Some(e) => (Conn::Failing, e),
            None => (
                Conn::Ok,
                format!(
                    "{}{}",
                    if a.label.is_empty() {
                        "API key"
                    } else {
                        &a.label
                    },
                    kept(a.remote)
                ),
            ),
        },
    };
    let nodeclub = match crate::nodeclub::account() {
        None => (Conn::Off, "No NodeClub key: add one on Home.".to_string()),
        Some(a) => match a.error {
            Some(e) => (Conn::Failing, e),
            None => (
                Conn::Ok,
                format!(
                    "{}{}",
                    if a.label.ends_with(" keys") {
                        a.label.clone()
                    } else {
                        format!("key {}", a.label)
                    },
                    kept(a.remote)
                ),
            ),
        },
    };
    let refuseless = match crate::refuseless::account() {
        None => (Conn::Off, "No Refuseless key: add one on Home.".to_string()),
        Some(a) => match a.error {
            Some(e) => (Conn::Failing, e),
            None => (
                Conn::Ok,
                format!(
                    "{}{}",
                    if a.label.ends_with(" keys") {
                        a.label.clone()
                    } else {
                        format!("key {}", a.label)
                    },
                    kept(a.remote)
                ),
            ),
        },
    };
    let muse = match crate::musecode::account() {
        None => (Conn::Off, "No Muse Code key: add one on Home.".to_string()),
        Some(a) => match a.error {
            Some(e) => (Conn::Failing, e),
            None => (
                Conn::Ok,
                format!(
                    "{}{}",
                    if a.label.ends_with(" keys") {
                        a.label.clone()
                    } else {
                        format!("key {}", a.label)
                    },
                    kept(a.remote)
                ),
            ),
        },
    };
    let freebuff = match freebuff::account() {
        None => (
            Conn::Off,
            "No Freebuff account: add one on Home.".to_string(),
        ),
        Some((email, _)) => match freebuff::error() {
            Some(e) => (Conn::Failing, e),
            None => (
                Conn::Ok,
                if email.is_empty() {
                    "Signed in".to_string()
                } else {
                    email
                },
            ),
        },
    };
    // Its server is the user's to start: what is known here is that one was
    // added, and where it is.
    let llama = match local::server() {
        None => (
            Conn::Off,
            "No local llama.cpp server: add one on Home.".to_string(),
        ),
        Some(s) => (
            Conn::Ok,
            format!(
                "{}, {} context, {}",
                s.display_name(),
                local::context_text(s.window()),
                s.url
            ),
        ),
    };
    [
        ("Claude", claude.0, claude.1),
        ("Codex", codex.0, codex.1),
        ("Antigravity", agy.0, agy.1),
        ("OpenRouter", router.0, router.1),
        ("Muse Code", muse.0, muse.1),
        ("NodeClub", nodeclub.0, nodeclub.1),
        ("Freebuff", freebuff.0, freebuff.1),
        ("Refuseless", refuseless.0, refuseless.1),
        ("Local", llama.0, llama.1),
    ]
}

/// The zoom factors Oak takes, as egui's own Ctrl+Plus and Ctrl+Minus.
const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.2..=5.0;

/// A saved zoom factor made safe to apply: within `ZOOM_RANGE`, 100% when
/// it is no number.
pub(crate) fn saved_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end())
    } else {
        1.0
    }
}

impl App {
    /// Keeps the interface's zoom in settings.json: egui changes it itself
    /// (Ctrl+Plus, Ctrl+=, Ctrl+Minus, Ctrl+0, at the end of a frame) and the
    /// Settings row asks for it, so each frame saves what is new since the
    /// last one.
    pub(crate) fn sync_zoom(&mut self, ctx: &egui::Context) {
        let z = ctx.zoom_factor();
        if z != self.settings.zoom {
            self.settings.zoom = z;
            store::save_settings(&self.settings);
        }
    }
}

/// The Zoom row: smaller, the zoom, larger and back to 100%, with the
/// keyboard shortcuts on hover.
fn zoom_row(ui: &mut Ui, zoom: f32) {
    let ctx = ui.ctx().clone();
    let shortcut = |s: &egui::KeyboardShortcut| ctx.format_shortcut(s);
    use egui::gui_zoom::kb_shortcuts as kb;
    ui.horizontal(|ui| {
        let out = ui.add_enabled(
            zoom > *ZOOM_RANGE.start() + 0.01,
            egui::Button::new("−").min_size(vec2(26.0, 0.0)),
        );
        if out
            .on_hover_text(format!("Zoom out ({})", shortcut(&kb::ZOOM_OUT)))
            .clicked()
        {
            egui::gui_zoom::zoom_out(&ctx);
        }
        let percent = format!("{:.0}%", zoom * 100.0);
        ui.add_sized(
            vec2(44.0, ui.spacing().interact_size.y),
            egui::Label::new(percent),
        );
        let into = ui.add_enabled(
            zoom < *ZOOM_RANGE.end() - 0.01,
            egui::Button::new("+").min_size(vec2(26.0, 0.0)),
        );
        if into
            .on_hover_text(format!("Zoom in ({})", shortcut(&kb::ZOOM_IN)))
            .clicked()
        {
            egui::gui_zoom::zoom_in(&ctx);
        }
        let reset = ui.add_enabled(zoom != 1.0, egui::Button::new("Reset"));
        if reset
            .on_hover_text(format!("100% ({})", shortcut(&kb::ZOOM_RESET)))
            .clicked()
        {
            ctx.set_zoom_factor(1.0);
        }
    });
}

/// One setting's caption and its options in a row (`tip` on the row), in
/// the settings grid. True when another was picked.
fn option_row<T: PartialEq + Clone>(
    ui: &mut Ui,
    t: &Theme,
    label: &str,
    value: &mut T,
    choices: impl IntoIterator<Item = (T, &'static str)>,
    tip: Option<&str>,
) -> bool {
    caption(ui, label, t.ink3);
    let before = value.clone();
    let r = ui.horizontal(|ui| {
        for (v, label) in choices {
            ui.selectable_value(value, v, label);
        }
    });
    if let Some(tip) = tip {
        r.response.on_hover_text(tip);
    }
    ui.end_row();
    before != *value
}

/// Dollars per million tokens, as short as it reads: "0.1", "0.035", "15".
fn per_m(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A switch's choices.
const OFF_ON: [(bool, &str); 2] = [(false, "Off"), (true, "On")];

/// A dot (green: working, red: failing, grey: not set up) and a name.
fn status_dot(ui: &mut Ui, t: &Theme, name: &str, state: Conn) -> egui::Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        let color = match state {
            Conn::Ok => hex(0x5fb36b),
            Conn::Failing => t.error,
            Conn::Off => t.ink4,
        };
        ui.painter().circle_filled(r.center(), 4.0, color);
        ui.label(RichText::new(name).color(if state == Conn::Off { t.ink3 } else { t.ink }));
    })
    .response
}

#[cfg(test)]
mod tests {
    /// A saved zoom is applied as it was, within egui's own limits, and an
    /// unusable one is 100%.
    #[test]
    fn saved_zoom_is_kept_within_range() {
        assert_eq!(super::saved_zoom(1.3), 1.3);
        assert_eq!(super::saved_zoom(0.0), 0.2);
        assert_eq!(super::saved_zoom(40.0), 5.0);
        assert_eq!(super::saved_zoom(f32::NAN), 1.0);
    }
}
