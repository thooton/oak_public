//! The New screen: where a conversation starts (its folder, model, effort
//! and system prompt), or a cloud session, or what a plugin adds
//! (`oak_sdk::NewKind`).

use super::*;

impl App {
    /// What a plugin adds that the New screen is set to start, if it is
    /// (and the plugin is loaded).
    pub(crate) fn new_kind(&self) -> Option<&'static Arc<dyn oak_sdk::NewKind>> {
        let id = self.new_kind.as_deref()?;
        crate::plugins::parts().new_kinds.iter().find(|k| k.id() == id)
    }

    /// The screen shown is a plugin's: one of its tabs, or the New screen
    /// starting what it adds.
    pub(crate) fn plugin_screen(&self) -> bool {
        match &self.screen {
            Screen::Chat(id) => crate::plugins::tab_kind(id).is_some(),
            Screen::New => self.new_kind().is_some(),
        }
    }

    /// It scrolls, so all of a long form (many models) can be reached
    /// above the message box.
    pub(crate) fn new_screen(&mut self, ui: &mut Ui, t: &Theme) {
        let top = ui.available_height() * 0.14;
        ScrollArea::vertical().id_salt("new-screen").auto_shrink([false, false]).show(ui, |ui| column(ui, 640.0, |ui| {
            ui.add_space(top);
            let kind = self.new_kind();
            ui.heading(kind.map_or_else(|| "New conversation".to_string(), |k| k.heading()));
            ui.add_space(16.0);
            self.where_row(ui);
            ui.add_space(18.0);
            if let Some(kind) = self.new_kind() {
                kind.form(ui, self);
                ui.add_space(16.0);
                return;
            }
            if self.new_cloud {
                let provider = self.new_provider;
                cloud_new_info(ui, t, provider);
                if let Some(backend) = crate::plugins::parts().cloud_backends.iter().find(|b| !provider.is_own() && b.id() == provider.id()) {
                    ui.add_space(8.0);
                    backend.form(ui, self);
                }
                return;
            }
            self.local_rows(ui, t);
            ui.add_space(16.0);
            self.role_row(ui, t);
            ui.add_space(16.0);
        }));
    }

    /// The rows of a conversation on this machine: its folder, model and
    /// effort.
    pub(crate) fn local_rows(&mut self, ui: &mut Ui, t: &Theme) {
        self.folder_row(ui, t);
        ui.add_space(16.0);
        self.model_rows(ui, t);
        ui.add_space(16.0);
        self.effort_row(ui, t);
    }

    /// Where it runs: a local folder or a cloud session (Oak's, or through
    /// a plugin's service), or what a plugin starts instead.
    fn where_row(&mut self, ui: &mut Ui) {
        let choice = (self.new_cloud, self.new_provider, self.new_kind.clone());
        let kind = self.new_kind().map(|k| k.id());
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(!self.new_cloud && kind.is_none(), "Local folder").on_hover_text("Runs on this machine").clicked() {
                self.new_cloud = false;
                self.new_kind = None;
            }
            for provider in cloud::Provider::all() {
                let (icon, text) = provider.icon();
                // (An icon of the text font is a letter: no icon font for it.)
                let label = if text { format!("{icon} {}", provider.name()) } else { format!("{icon}  {}", provider.name()) };
                let mut r = ui.selectable_label(self.new_cloud && kind.is_none() && self.new_provider == provider, label);
                let tip = provider.tip();
                if !tip.is_empty() {
                    r = r.on_hover_text(tip);
                }
                if r.clicked() {
                    self.new_cloud = true;
                    self.new_kind = None;
                    self.new_provider = provider;
                }
            }
            for k in &crate::plugins::parts().new_kinds {
                let (id, icon) = (k.id(), k.icon());
                let label = if icon.is_empty() { k.label() } else { format!("{icon} {}", k.label()) };
                let mut r = ui.selectable_label(kind.as_deref() == Some(id.as_str()), label);
                let tip = k.tip();
                if !tip.is_empty() {
                    r = r.on_hover_text(tip);
                }
                if r.clicked() {
                    self.new_kind = Some(id);
                    self.new_cloud = false;
                }
            }
        });
        // A hint is about the choice it was given for.
        if choice != (self.new_cloud, self.new_provider, self.new_kind.clone()) {
            self.hint = None;
        }
    }

    /// The folder, typed or chosen.
    fn folder_row(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "Folder", t.ink3);
        ui.horizontal(|ui| {
            let w = ui.available_width() - 100.0;
            crate::text_menu::add(ui, TextEdit::singleline(&mut self.settings.folder).desired_width(w).hint_text(if cfg!(windows) { "C:\\path\\to\\project" } else { "/path/to/project" }));
            if ui.button("Choose…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.settings.folder = dir.display().to_string();
                store::save_settings(&self.settings);
            }
        });
    }

    /// The providers with models, then the chosen one's models (once they
    /// are loaded). One saved from before is the model's.
    fn model_rows(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "Provider", t.ink3);
        let models = self.models.lock().unwrap().clone();
        let list = match models {
            None => {
                crate::widgets::working(ui, t, "Loading models…");
                return;
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("Couldn't load models: {e}")).color(t.error));
                if ui.button("Retry").clicked() {
                    self.fetch_models(ui.ctx());
                }
                return;
            }
            Some(Ok(list)) => list,
        };
        let providers: Vec<api::Provider> = api::Provider::ALL.into_iter().filter(|p| list.iter().any(|m| m.provider == *p)).collect();
        let mut provider = self.settings.provider.unwrap_or_else(|| api::Provider::of(&self.settings.model));
        if !providers.contains(&provider)
            && let Some(&p) = providers.first()
        {
            provider = p;
        }
        ui.horizontal_wrapped(|ui| {
            for &p in &providers {
                let on = p == provider;
                let b = Button::image_and_text(crate::providers_view::logo_image(ui.ctx(), p, 14.0), p.name()).selected(on);
                if ui.add(b).clicked() && !on {
                    provider = p;
                }
            }
        });
        let group: Vec<&api::Model> = list.iter().filter(|m| m.provider == provider).collect();
        if !group.iter().any(|m| m.id == self.settings.model)
            && let Some(m) = group.first()
        {
            self.settings.model = m.id.clone();
        }
        if self.settings.provider != Some(provider) {
            self.settings.provider = Some(provider);
            store::save_settings(&self.settings);
        }
        ui.add_space(12.0);
        caption(ui, "Model", t.ink3);
        ui.horizontal_wrapped(|ui| {
            for m in group {
                let on = self.settings.model == m.id;
                let resp = ui.selectable_label(on, crate::session_box::model_label(m));
                let resp = match crate::session_box::model_tip(m) {
                    Some(tip) => resp.on_hover_text(tip),
                    None => resp,
                };
                if resp.clicked() && !on {
                    self.settings.model = m.id.clone();
                    store::save_settings(&self.settings);
                }
            }
        });
        if provider == api::Provider::Claude
            && let Some(e) = api::claude_models_error()
        {
            ui.horizontal_wrapped(|ui| {
                caption(ui, &format!("Couldn't load the Claude models: {e}"), t.error);
                if ui.small_button("Retry").clicked() {
                    self.fetch_models(ui.ctx());
                }
            });
        }
    }

    /// The model's own levels; a saved one it lacks shows (and goes) as its
    /// nearest (`agent::effort_for`), and stays saved for models that have
    /// it.
    fn effort_row(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "Effort", t.ink3);
        ui.horizontal(|ui| {
            let shown = agent::effort_for(&self.settings.model, &self.settings.effort);
            let levels = agent::efforts(&self.settings.model);
            if levels.is_empty() {
                ui.label(RichText::new("Not supported by this model").color(t.ink3));
            }
            for e in levels {
                if ui.selectable_label(shown == e, e).clicked() && self.settings.effort != e {
                    self.settings.effort = e.to_string();
                    store::save_settings(&self.settings);
                }
            }
        });
    }

    /// Who the model is and how it works with the user; the tool
    /// instructions are the same for all (`agent::prompt::ROLES`).
    fn role_row(&mut self, ui: &mut Ui, t: &Theme) {
        caption(ui, "System prompt", t.ink3);
        ui.horizontal_wrapped(|ui| {
            if crate::freebuff::serves(&self.settings.model) {
                ui.label(RichText::new("Not used by this model: Freebuff sends its own").color(t.ink3));
                return;
            }
            let shown = agent::prompt::role(&self.settings.role).id;
            for r in agent::prompt::offered() {
                if ui.selectable_label(shown == r.id, r.name).clicked() && shown != r.id {
                    self.settings.role = r.id.to_string();
                    store::save_settings(&self.settings);
                }
            }
        });
    }

    /// Starts a conversation with a message, a goal, or both.
    pub(crate) fn start_conversation(&mut self, text: String, images: Vec<Value>, cmd: Option<Command>, ctx: &egui::Context) {
        self.start_conversation_as(text, images, cmd, None, ctx);
    }

    /// Starts a conversation; with `named`, one a plugin starts: in that
    /// role, and by that title if it gives one (never retitled then).
    /// Returns its id, or None with the hint saying why it didn't start.
    pub(crate) fn start_conversation_as(&mut self, text: String, images: Vec<Value>, cmd: Option<Command>, named: Option<(String, String)>, ctx: &egui::Context) -> Option<String> {
        let typed = self.settings.folder.trim();
        if typed.is_empty() {
            self.hint = Some("Folder required".into());
            return None;
        }
        let folder = instructions::expand_home(typed, &instructions::user_home());
        if !folder.is_dir() {
            self.hint = Some(format!("Folder not found: {}", folder.display()));
            return None;
        }
        if !matches!(&*self.models.lock().unwrap(), Some(Ok(_))) || self.settings.model.is_empty() {
            self.hint = Some("Model required".into());
            return None;
        }
        store::save_settings(&self.settings);
        let now = api::now_ms();
        let mut conv = Conversation {
            id: now.to_string(),
            title: String::new(),
            folder,
            provider: Some(self.settings.provider.unwrap_or_else(|| api::Provider::of(&self.settings.model))),
            model: self.settings.model.clone(),
            effort: agent::effort_for(&self.settings.model, &self.settings.effort),
            role: agent::prompt::role(named.as_ref().map_or(self.settings.role.as_str(), |(role, _)| role.as_str())).id.to_string(),
            messages: Default::default(),
            last_message_at: now,
            next_task_id: 0,
            rolling: rolling::State { off: !self.settings.rolling, ..Default::default() },
            ..Default::default()
        };
        let id = conv.id.clone();
        if let Some(Command::Goal(g)) = &cmd {
            conv.title = g.lines().next().unwrap_or("").head(60);
        }
        if let Some((_, title)) = named
            && !title.is_empty()
        {
            conv.title = title;
            conv.titled = true;
        }
        let s = Session::start(conv, ctx.clone());
        if let Some(cmd) = cmd {
            s.command(cmd);
        }
        if !text.is_empty() || !images.is_empty() {
            s.send(text, images);
        }
        self.sessions.insert(id.clone(), s);
        self.open(id.clone());
        Some(id)
    }
}
