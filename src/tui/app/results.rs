use super::*;

impl App {
    pub(super) fn task_done(&mut self, done: Done) {
        if self.work.take_current(done.ticket).is_some() {
            self.complete(done.result);
        }
    }

    pub(super) fn complete(&mut self, result: TaskResult) {
        match result {
            TaskResult::Remembered(result) => {
                if let Err(warning) = result {
                    self.set_status(StatusKind::Warning, warning);
                }
            }
            TaskResult::Closed => {}
            TaskResult::ConversationCopied(text) => self.effects.push(Effect::Copy(
                text,
                "the retained conversation (older content may be omitted)".into(),
            )),
            TaskResult::Failed(error) => self.set_status(StatusKind::Error, error),
            TaskResult::Catalog {
                purpose,
                name,
                result,
            } => self.models_loaded(purpose, name, result),
            TaskResult::Opened(result) => match result {
                Ok(next) => self.attach_session(*next),
                Err(error) => self.set_status(StatusKind::Error, error),
            },
            TaskResult::InputPrepared(preparation) => self.input_prepared(*preparation),
            TaskResult::Resumed(result) => match result {
                Ok(resumed) => self.open(resumed.settings, Some(resumed.record)),
                Err(error) => self.set_status(StatusKind::Error, error),
            },
            TaskResult::Sessions(purpose, result) => {
                self.sessions_loaded(purpose, result);
            }
            TaskResult::Providers(route, result) => match result {
                Ok(mut entries) => {
                    if let Some(route) = route {
                        if route == picker::Route::Search {
                            entries.retain(|entry| {
                                entry.ready && entry.provider.capabilities().web_search
                            });
                        }
                        if entries.is_empty() {
                            self.set_status(
                                StatusKind::Info,
                                "no signed-in provider offers web search",
                            );
                        } else if let Some(Surface::Picker(picker)) = &mut self.surface {
                            picker.route(route, entries);
                        }
                    } else {
                        self.set_picker(Some(Picker::providers(entries, self.session.is_some())));
                    }
                }
                Err(error) => self.set_status(StatusKind::Error, error),
            },
            TaskResult::ImageRead(result) => match result {
                Ok(Some(image)) => self.attach(image),
                Ok(None) => self.set_status(StatusKind::Info, "no image in the clipboard"),
                Err(error) => self.set_status(StatusKind::Error, error),
            },
            TaskResult::FilesIndexed(result) => match result {
                Ok(index) => self.files_indexed(index),
                Err(error) => self.set_status(StatusKind::Error, error),
            },
            TaskResult::McpToggled(name, result) => self.mcp_done(&name, result),
            TaskResult::Deleted(id, result) => self.deleted(&id, result),
        }
    }

    pub(super) fn attach_session(&mut self, opened: crate::tui::tasks::Opened) {
        let crate::tui::tasks::Opened {
            mut session,
            id,
            pastes,
            transcript,
            recent_inputs,
        } = opened;
        if self.session.is_some() {
            for kind in [
                Kind::Catalog,
                Kind::Providers,
                Kind::Sessions,
                Kind::Image,
                Kind::Files,
                Kind::Prepare,
                Kind::Open,
            ] {
                self.cancel_task(kind);
            }
            let mut work = std::mem::take(&mut self.work);
            work.preferences = None;
            let effects = std::mem::take(&mut self.effects);
            *self = Self::new(
                session.settings().clone(),
                session.preferences(),
                self.shared.clone(),
                self.geometry.size,
                self.first_run,
            );
            self.work = work;
            self.effects = effects;
        }

        self.info.settings = session.settings().clone();
        self.info.name = session.journal().name().map(Clean::new);
        self.info.recording_stopped = session.journal().recording_warning().is_some();
        self.info.confinement = session.tool_context().confinement();
        match pastes {
            Ok(pastes) => {
                self.drafts.next_paste = pastes.next;
                self.drafts.pastes = Some(pastes.artifacts);
            }
            Err(error) => self.set_status(StatusKind::Error, error),
        }
        for text in recent_inputs {
            self.remember_input(text);
        }
        session.interactive();
        self.info.accounting = session.accounting();
        self.transcript = transcript;
        self.info.preferences = session.preferences();
        self.session = Some(SessionView {
            id: id.clone(),
            inbox: session.inbox(),
            live: session.live(),
            context: session.tool_context(),
            jobs: session.jobs(),
        });
        if self.first_run {
            self.first_run = false;
            self.set_picker(Some(Picker::first_run(
                self.info.preferences.effective.clone(),
            )));
            self.load_context();
        }
        self.start_task(Task::Remember(
            self.shared.state.clone(),
            self.info.settings.clone(),
        ));
        let close = self.work.ticket(Kind::Close(self.work.next_ticket));
        self.effects
            .push(Effect::Open(Box::new((session, id)), close));
    }

    pub(super) fn models_loaded(
        &mut self,
        purpose: CatalogUse,
        name: String,
        result: Result<Vec<Model>, String>,
    ) {
        match purpose {
            CatalogUse::Settings => {
                if name == self.info.settings.provider.name() {
                    match result {
                        Ok(models) => {
                            if let Some(Surface::Picker(picker)) = &mut self.surface {
                                let model = models
                                    .into_iter()
                                    .find(|m| m.id == self.info.settings.model);
                                picker.context_loaded(
                                    name,
                                    self.info.settings.model.clone(),
                                    model,
                                );
                            }
                        }
                        Err(error) => {
                            if let Some(Surface::Picker(picker)) = &mut self.surface {
                                picker.context_loaded(name, self.info.settings.model.clone(), None);
                            }
                            self.set_status(
                                StatusKind::Error,
                                format!("cannot list models: {error}"),
                            );
                        }
                    }
                }
                return;
            }
            CatalogUse::Picker => {
                let error = result.as_ref().err().cloned();
                if let Some(Surface::Picker(picker)) = &mut self.surface
                    && picker.models_loaded(&name, result)
                    && let Some(error) = error
                {
                    self.transcript.ui_notice(
                        StatusKind::Error,
                        &Clean::new(&format!("cannot list models for {name}: {error}")),
                    );
                }
                return;
            }
            CatalogUse::Effort | CatalogUse::Step { .. }
                if name != self.info.settings.provider.name() =>
            {
                return;
            }
            CatalogUse::Effort | CatalogUse::Step { .. } => {}
        }
        let current = &self.info.settings;
        match result.map(|models| models.into_iter().find(|model| model.id == current.model)) {
            Ok(Some(model)) if !model.efforts.is_empty() => match purpose {
                CatalogUse::Step { up } => self.step_effort(&model, up),
                _ => self.set_picker(Some(Picker::efforts(
                    current.provider.clone(),
                    model,
                    current.reasoning_effort.as_deref(),
                ))),
            },
            Ok(_) => self.set_status(
                StatusKind::Info,
                format!(
                    "{} lists no reasoning efforts for {}",
                    current.provider.name(),
                    current.model
                ),
            ),
            Err(error) => {
                self.set_status(StatusKind::Error, format!("cannot list models: {error}"))
            }
        }
    }

    pub(super) fn sessions_loaded(
        &mut self,
        purpose: Purpose,
        result: Result<Vec<crate::record::Summary>, String>,
    ) {
        let app = &mut *self;
        match result {
            Ok(mut list) => {
                list.retain(|session| {
                    session.id != app.session.as_ref().map(|s| s.id.as_str()).unwrap_or("")
                });
                app.set_picker(Some(Picker::sessions(purpose, list)));
            }
            Err(error) => {
                app.set_status(StatusKind::Error, format!("cannot list sessions: {error}"))
            }
        }
    }

    pub(super) fn deleted(&mut self, id: &str, result: Result<(), String>) {
        self.status.clear();
        match result {
            Ok(()) => {
                if let Some(Surface::Picker(picker)) = &mut self.surface
                    && !picker.remove(id)
                {
                    self.set_picker(None);
                }
            }
            Err(error) => self.set_status(StatusKind::Error, format!("cannot delete: {error}")),
        }
    }

    pub(super) fn mcp_done(&mut self, name: &str, result: Result<(bool, Option<String>), String>) {
        match result {
            Ok((enabled, warning)) => {
                let state = if enabled {
                    "enabled; it is starting"
                } else {
                    "disabled in this session and future sessions"
                };
                self.status.clear();
                if let Some(warning) = warning {
                    self.set_status(StatusKind::Warning, format!("{name} {state}; {warning}"));
                }
            }
            Err(error) => self.set_status(StatusKind::Error, error),
        }
    }

    pub(super) fn select_session(&mut self, id: String, cwd: PathBuf) {
        if self.active_task(Kind::Mcp) {
            self.set_status(StatusKind::Info, "wait for the MCP change to finish saving");
            return;
        }
        if cwd != self.shared.opener.cwd {
            if !cwd.is_dir() {
                self.set_status(
                    StatusKind::Error,
                    format!(
                        "session directory no longer exists; run after restoring it: {}",
                        crate::relaunch::command(&cwd, &id)
                    ),
                );
                return;
            }
            match std::env::current_exe() {
                Ok(exe) => self.effects.push(Effect::Exec(crate::relaunch::Plan::new(
                    cwd,
                    &id,
                    exe,
                    &self.info.settings,
                    &self.info.preferences.effective,
                ))),
                Err(error) => self.set_status(StatusKind::Error, error.to_string()),
            }
        } else {
            self.start_task(Task::Resume {
                sessions: self.shared.sessions.clone(),
                id,
                config: self.shared.loader.config.clone(),
            });
        }
    }

    pub(super) fn open(&mut self, settings: Settings, resumed: Option<crate::record::Resumed>) {
        if self.active_task(Kind::Mcp) {
            self.set_status(StatusKind::Info, "wait for the MCP change to finish saving");
            return;
        }
        if self.active_task(Kind::Open) {
            self.set_status(StatusKind::Info, "session opening is still running");
            return;
        }
        self.start_task(Task::Open(Box::new(crate::tui::tasks::Opening {
            opener: self.shared.opener.clone(),
            settings,
            preferences: self.info.preferences.clone(),
            resumed,
        })));
    }

    pub(super) fn load_models(&mut self, provider: Provider, purpose: CatalogUse) {
        self.start_task(Task::Catalog {
            loader: self.shared.loader.clone(),
            provider,
            purpose,
        });
    }

    pub(super) fn load_context(&mut self) {
        self.load_models(self.info.settings.provider.clone(), CatalogUse::Settings);
    }

    pub(super) fn load_providers(&mut self, route: Option<picker::Route>) {
        if self.work.preferences.is_some() {
            self.set_status(StatusKind::Info, "wait for settings to finish saving");
            return;
        }
        self.cancel_picker_tasks();
        self.start_task(Task::Providers(self.shared.loader.clone(), route));
    }

    fn step_effort(&mut self, model: &crate::provider::Model, up: bool) {
        let current = &self.info.settings;
        let at = current
            .reasoning_effort
            .as_ref()
            .or(model.default_effort.as_ref())
            .and_then(|effort| model.efforts.iter().position(|known| known == effort));
        let next = match (at, up) {
            (None, _) => Some(0),
            (Some(at), true) => Some(at + 1).filter(|&next| next < model.efforts.len()),
            (Some(at), false) => at.checked_sub(1),
        };
        match next {
            Some(next) => {
                let settings = Settings {
                    reasoning_effort: Some(model.efforts[next].clone()),
                    ..current.clone()
                };
                self.switch(settings);
            }
            None => self.set_status(
                StatusKind::Info,
                if up {
                    "already at the highest effort"
                } else {
                    "already at the lowest effort"
                },
            ),
        }
    }

    pub(super) fn switch(&mut self, settings: Settings) {
        if self.session.is_none() {
            self.open(settings, None);
        } else {
            self.send_command(Command::Switch(settings));
        }
    }

    pub(super) fn toggle_mcp(&mut self, name: String) {
        if self.active_task(Kind::Open) {
            self.set_status(StatusKind::Info, "wait for session opening to finish");
            return;
        }
        if self.active_task(Kind::Mcp) {
            self.set_status(StatusKind::Info, "MCP toggle is still running");
            return;
        }
        if let Some(session) = &self.session {
            self.start_task(Task::Mcp {
                name,
                config: self.shared.loader.config.clone(),
                registry: session.context.mcp(),
            });
        }
    }

    pub(super) fn delete(&mut self, id: &str) {
        if self.active_task(Kind::Delete) {
            self.set_status(StatusKind::Info, "session deletion is still running");
            return;
        }
        self.set_status(
            StatusKind::Info,
            "deleting session… deletion may finish after you leave",
        );
        self.start_task(Task::Delete(self.shared.sessions.clone(), id.to_owned()));
    }

    pub(super) fn prefer(&mut self, change: Change) {
        if self.turn.is_some() || self.active_task(Kind::Mcp) {
            self.set_status(
                StatusKind::Info,
                "finish the turn or MCP change before changing settings",
            );
            return;
        }
        if self.work.preferences.is_some() {
            self.set_status(StatusKind::Info, "wait for settings to finish saving");
            return;
        }
        let ticket = self.work.ticket(Kind::Preferences);
        self.work.preferences = Some(ticket);
        if let Some(Surface::Picker(picker)) = &mut self.surface {
            picker.preference_accepted(&change, ticket);
        }
        self.send_command(Command::Prefer(change, ticket));
    }
}
