use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Slash {
    New,
    Rename,
    Resume,
    Delete,
    Settings,
    Model,
    Effort,
    Copy,
    Mcps,
    Compact,
    Rewind,
    Quit,
}
pub(super) const COMMANDS: [(Slash, &str, &str); 12] = [
    (Slash::New, "new", "start a new session"),
    (Slash::Rename, "rename", "name this session"),
    (Slash::Resume, "resume", "continue an earlier session"),
    (Slash::Delete, "delete", "delete an earlier session"),
    (Slash::Settings, "settings", "change Bart preferences"),
    (Slash::Model, "model", "choose the provider and model"),
    (Slash::Effort, "effort", "choose the reasoning effort"),
    (Slash::Copy, "copy", "copy the last answer"),
    (
        Slash::Mcps,
        "mcps",
        "MCP servers and their tools; enable or disable one",
    ),
    (
        Slash::Compact,
        "compact",
        "compact context and retain Python state",
    ),
    (
        Slash::Rewind,
        "rewind",
        "remove a message you sent, and what followed it",
    ),
    (Slash::Quit, "quit", "leave bart"),
];

pub(super) fn parse_slash(text: &str) -> Option<(Slash, Option<String>)> {
    let text = text.strip_prefix('/')?;
    let (name, argument) = text
        .split_once(char::is_whitespace)
        .map_or((text, None), |(name, rest)| {
            (name, (!rest.trim().is_empty()).then(|| rest.to_owned()))
        });
    COMMANDS
        .iter()
        .find(|(_, known, _)| *known == name)
        .map(|(command, _, _)| (*command, argument))
}

impl App {
    pub(super) fn input(&mut self, event: Event) {
        match event {
            Event::Key(mut key) if key.kind == KeyEventKind::Press => {
                if key.code == KeyCode::Char('[') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    key.code = KeyCode::Esc;
                    key.modifiers = KeyModifiers::NONE;
                }
                if self.global_key(key) {
                    return;
                }
                if key.code == KeyCode::Esc
                    && self
                        .turn
                        .as_ref()
                        .is_some_and(|turn| turn.progress.waiting_for_reset())
                {
                    self.set_picker(None);
                    self.interrupt(false);
                    return;
                }
                if let Some(Surface::Viewer(viewer, _)) = &mut self.surface {
                    let live = &self.session.as_ref().expect("viewer session").live;
                    if let Some(effect) =
                        viewer.key(key, self.transcript.details(), live, &mut self.status)
                    {
                        if matches!(effect, Effect::Clear) {
                            self.close_details();
                        }
                        self.effects.push(effect);
                    }
                    return;
                }
                if key.code == KeyCode::Esc && self.surface.is_none() {
                    self.cancel_picker_tasks();
                }
                if self.surface.is_none() {
                    match key.code {
                        KeyCode::PageUp => {
                            self.scrollback.scroll(false);
                            return;
                        }
                        KeyCode::PageDown => {
                            self.scrollback.scroll(true);
                            return;
                        }
                        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.scrollback.follow();
                            return;
                        }
                        _ => {}
                    }
                    self.scrollback.clear_selection();
                }
                self.composer_key(key);
                if self.wants_files()
                    && !self.active_task(Kind::Files)
                    && let Some(session) = &self.session
                {
                    self.start_task(Task::Files(session.context.cwd.clone()));
                }
            }
            Event::Paste(text) => {
                if let Some(Surface::Viewer(viewer, _)) = &mut self.surface {
                    if let Err(reason) = viewer.paste(&text) {
                        self.set_status(StatusKind::Info, reason);
                    }
                } else {
                    self.paste_visible(&text);
                }
            }
            Event::Mouse(mouse) => self.mouse(mouse),
            Event::FocusGained => self.effects.push(Effect::Clear),
            Event::Resize(..) => self.geometry.resized_at = Some(Instant::now()),
            _ => {}
        }
    }

    fn copy_conversation(&mut self) {
        self.start_task(Task::CopyConversation(self.transcript.copy_sources()));
    }

    fn global_key(&mut self, key: KeyEvent) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && self.surface.is_none()
        {
            self.copy_conversation();
            return true;
        }
        match key.code {
            KeyCode::Char('c') => {
                if self
                    .quit_armed
                    .is_some_and(|armed| armed.elapsed() < QUIT_WINDOW)
                {
                    self.effects.push(Effect::Quit);
                    return true;
                }
                self.quit_armed = Some(Instant::now());
                self.cancel_task(Kind::Catalog);
                if let Some(Surface::Picker(picker)) = &self.surface {
                    if picker.cancellable() {
                        self.set_picker(None);
                    }
                } else if self.turn.is_some() {
                    self.interrupt(false);
                } else if self.surface.is_none() {
                    self.cancel_task(Kind::Image);
                    self.drafts.composer.clear();
                }
                self.set_status(StatusKind::Info, "press ctrl+c again to quit");
            }
            KeyCode::Char('o') => {
                if matches!(self.surface, Some(Surface::Viewer(..))) {
                    self.close_details();
                    self.effects.push(Effect::Clear);
                } else if self.surface.is_none() {
                    self.open_details(None);
                } else {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }

    fn review_key(&mut self, key: KeyEvent, frame: Option<view::FrameStamp>) -> bool {
        if let Some(Surface::Approval(answer)) = &mut self.surface
            && answer.answer.pending()
        {
            match answer.confirm.key(key, frame) {
                outcome @ (ConfirmOutcome::Accept | ConfirmOutcome::Cancel) => {
                    let allow = matches!(outcome, ConfirmOutcome::Accept);
                    self.effects.push(Effect::Approve {
                        answer: answer.answer.clone(),
                        allow,
                    });
                    self.surface = None;
                }
                ConfirmOutcome::Stay => {}
            }
            return key.code != KeyCode::Char('c')
                || !key.modifiers.contains(KeyModifiers::CONTROL);
        }

        false
    }

    fn picker_or_command_key(&mut self, key: KeyEvent, frame: Option<view::FrameStamp>) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some(Surface::Picker(picker)) = &mut self.surface
            && !(control && key.code == KeyCode::Char('c'))
        {
            match picker.key(key, frame) {
                Choice::Stay | Choice::Activate(_) => (),
                Choice::Back => {
                    self.cancel_task(Kind::Catalog);
                    if self.needs_context() {
                        self.load_context()
                    }
                }
                Choice::Details(error) => self.open_details(Some(error)),
                Choice::Refused(reason) => {
                    self.set_status(StatusKind::Info, reason);
                }
                Choice::Load(provider) => self.load_models(provider, CatalogUse::Picker),
                Choice::Chosen(choice) => {
                    if self.session.is_some() {
                        self.set_picker(None);
                    }
                    self.switch(choice)
                }
                Choice::Cancelled => {
                    self.set_picker(None);
                }
                Choice::NeedsLogin(hint) => {
                    self.set_status(
                        StatusKind::Info,
                        format!("not logged in; run `{hint}` in a shell"),
                    );
                }
                Choice::Resume(id, cwd) => {
                    self.set_picker(None);
                    self.select_session(id, cwd)
                }
                Choice::Prefer(preferences) => self.prefer(preferences),
                Choice::Route(route) => self.load_providers(Some(route)),
                Choice::Delete(id) => self.delete(&id),
                Choice::Toggle(name) => self.toggle_mcp(name),
                Choice::Rewind(at) => {
                    self.set_picker(None);
                    self.send_command(Command::Rewind(at))
                }
            };
            return true;
        }
        if key.code == KeyCode::Enter
            && !alt
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && let Some((command, argument)) = parse_slash(self.drafts.composer.text())
        {
            self.cancel_task(Kind::Image);
            self.drafts.composer.clear();
            self.command(command, argument);
            return true;
        }
        let found = self.mentions();
        if !found.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.drafts.menu = self.drafts.menu.saturating_sub(1);
                    return true;
                }
                KeyCode::Down => {
                    self.drafts.menu = (self.drafts.menu + 1).min(found.len() - 1);
                    return true;
                }
                KeyCode::Enter | KeyCode::Tab
                    if !alt && !key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    let path = found[self.drafts.menu.min(found.len() - 1)].path.clone();
                    if !self.drafts.composer.complete_mention(&path) {
                        self.set_status(
                            StatusKind::Info,
                            "draft limit is 1 MiB; completion refused",
                        );
                    }
                    self.drafts.menu = 0;
                    return true;
                }
                KeyCode::Esc => {
                    self.drafts.mention_closed = true;
                    return true;
                }
                _ => {}
            }
        }
        let menu = self.menu();
        if !menu.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.drafts.menu = self.drafts.menu.saturating_sub(1);
                    return true;
                }
                KeyCode::Down => {
                    self.drafts.menu = (self.drafts.menu + 1).min(menu.len() - 1);
                    return true;
                }
                KeyCode::Enter | KeyCode::Tab
                    if !alt && !key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    let (command, _, _) = menu[self.drafts.menu.min(menu.len() - 1)];
                    self.cancel_task(Kind::Image);
                    self.drafts.composer.clear();
                    self.drafts.menu = 0;
                    self.command(command, None);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let frame = self.review_stamp();
        self.clear_picker_feedback();
        if self.review_key(key, frame) {
            return;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if key.code == KeyCode::Up
            && alt
            && !matches!(self.surface, Some(Surface::Picker(_)))
            && self.drafts.has_pending()
            && !self.drafts.composer.is_empty()
        {
            self.set_status(
                StatusKind::Info,
                "finish the current draft before recalling pending input",
            );
            return;
        }
        if self.picker_or_command_key(key, frame) {
            return;
        }
        match key.code {
            KeyCode::Esc if self.turn.is_some() => self.interrupt(true),
            KeyCode::Esc => self.cancel_task(Kind::Catalog),
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert("\n");
            }
            KeyCode::Char('j') if control => self.insert("\n"),
            KeyCode::Char('=' | '+') if alt => {
                self.load_models(
                    self.info.settings.provider.clone(),
                    CatalogUse::Step { up: true },
                );
            }
            KeyCode::Char('-') if alt => {
                self.load_models(
                    self.info.settings.provider.clone(),
                    CatalogUse::Step { up: false },
                );
            }
            KeyCode::Enter => {
                self.cancel_task(Kind::Catalog);
                self.prepare_or_commit(Delivery::Send);
            }
            KeyCode::Tab if self.turn.is_some() && !self.drafts.composer.is_empty() => {
                self.prepare_or_commit(Delivery::Queue)
            }
            KeyCode::Up
                if (key.modifiers.is_empty() || key.modifiers == KeyModifiers::ALT)
                    && self.drafts.composer.is_empty()
                    && self.drafts.has_pending() =>
            {
                self.recall_pending()
            }
            KeyCode::Up if alt => {}
            KeyCode::Up if self.drafts.composer.on_first_line() => self.recall(true),
            KeyCode::Down if self.drafts.composer.on_last_line() => self.recall(false),
            KeyCode::Up => self.drafts.composer.line_up(),
            KeyCode::Down => self.drafts.composer.line_down(),
            KeyCode::Char('v') if control => self.paste_image(),
            KeyCode::Char('w') if control => self.drafts.composer.delete_to_space(),
            KeyCode::Char('u') if control => self.drafts.composer.delete_to_start(),
            KeyCode::Char('k') if control => self.drafts.composer.delete_to_end(),
            KeyCode::Char('a') if control => self.drafts.composer.home(),
            KeyCode::Char('e') if control => self.drafts.composer.end(),
            // Some terminals send ^H for Backspace.
            KeyCode::Char('h') if control => self.drafts.composer.backspace(),
            KeyCode::Char('b') if alt => self.drafts.composer.word_left(),
            KeyCode::Char('f') if alt => self.drafts.composer.word_right(),
            KeyCode::Char('d') if alt => self.drafts.composer.delete_word_right(),
            KeyCode::Left if control || alt => self.drafts.composer.word_left(),
            KeyCode::Right if control || alt => self.drafts.composer.word_right(),
            KeyCode::Backspace if control || alt => self.drafts.composer.delete_word_left(),
            KeyCode::Delete if control || alt => self.drafts.composer.delete_word_right(),
            KeyCode::Backspace => self.drafts.composer.backspace(),
            KeyCode::Delete => self.drafts.composer.delete(),
            KeyCode::Left => self.drafts.composer.left(),
            KeyCode::Right => self.drafts.composer.right(),
            KeyCode::Home => self.drafts.composer.home(),
            KeyCode::End => self.drafts.composer.end(),
            KeyCode::Char(c) if !control && !alt => {
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            _ => {}
        }
    }

    fn mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        match Event::Mouse(mouse) {
            Event::Mouse(mouse) if matches!(self.surface, Some(Surface::Viewer(..))) => {
                let stamp = self.review_stamp();
                let Some(Surface::Viewer(viewer, _)) = &mut self.surface else {
                    return;
                };
                if let Some(text) = viewer.mouse(mouse, stamp) {
                    self.effects
                        .push(Effect::Copy(text, "the selection".into()));
                }
            }
            Event::Mouse(mouse) if self.surface.is_none() => {
                use crossterm::event::{MouseButton, MouseEventKind};
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.drafts.composer.clear_selection();
                        self.scrollback.clear_selection();
                        if self.drafts.composer.contains(mouse.column, mouse.row) {
                            self.drafts.composer.select(mouse.column, mouse.row, false);
                            return;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
                        if self.drafts.composer.selecting() =>
                    {
                        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
                            self.drafts.composer.select(mouse.column, mouse.row, true);
                        } else if let Some(text) = self.drafts.composer.finish_selection() {
                            self.effects
                                .push(Effect::Copy(text, "the selection".into()));
                        }
                        return;
                    }
                    _ => {}
                }
                let area = self.geometry.conversation;
                let row = mouse.row.saturating_sub(area.y);
                let column = usize::from(mouse.column.saturating_sub(area.x));
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scrollback.scroll_line(false),
                    MouseEventKind::ScrollDown => self.scrollback.scroll_line(true),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if area.contains((mouse.column, mouse.row).into()) {
                            self.scrollback.select(usize::from(row), column, false);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if mouse.row < area.y || mouse.row >= area.bottom() {
                            self.scrollback.select_beyond(mouse.row < area.y, column);
                        } else {
                            self.scrollback.select(usize::from(row), column, true);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(text) = self.scrollback.selected_text(&self.transcript) {
                            let chars = format!("{} chars", text.chars().count());
                            self.effects.push(Effect::Copy(text, chars));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn command(&mut self, command: Slash, argument: Option<String>) {
        if self.work.preferences.is_some() && command != Slash::Quit {
            self.set_status(StatusKind::Info, "wait for settings to finish saving");
            return;
        }
        if argument.is_some() && command != Slash::Rename {
            self.set_status(StatusKind::Info, "this command takes no argument");
            return;
        }
        self.cancel_picker_tasks();
        match command {
            Slash::Quit => self.effects.push(Effect::Quit),
            Slash::Mcps => {
                if let Some(session) = &self.session {
                    self.set_picker(Some(Picker::mcps(session.context.mcp())));
                }
            }
            Slash::Copy => {
                if self.transcript.last_answer().trim().is_empty() {
                    self.set_status(StatusKind::Info, "no answer yet");
                } else {
                    self.effects.push(Effect::Copy(
                        self.transcript.last_answer().trim().to_owned(),
                        "the last answer".into(),
                    ));
                }
            }
            _ if self.turn.is_some() => {
                self.set_status(StatusKind::Info, "finish or interrupt the turn first")
            }
            Slash::Rename => match argument {
                Some(name) => {
                    self.send_command(Command::Rename(name));
                    self.turn = Some(Turn::new());
                }
                None => self.set_status(StatusKind::Info, "type a name after /rename"),
            },
            Slash::Settings => {
                self.set_picker(Some(Picker::settings(
                    self.info.preferences.effective.clone(),
                )));
                self.load_context();
            }
            Slash::Compact => {
                self.send_command(Command::Compact);
                self.turn = Some(Turn::new());
            }
            Slash::Rewind => self.send_command(Command::Messages),
            Slash::New => self.open(self.info.settings.clone(), None),
            Slash::Resume | Slash::Delete => {
                self.start_task(Task::Sessions(
                    self.shared.sessions.clone(),
                    if command == Slash::Resume {
                        Purpose::Resume
                    } else {
                        Purpose::Delete
                    },
                ));
            }
            Slash::Model => self.load_providers(None),
            Slash::Effort => {
                self.load_models(self.info.settings.provider.clone(), CatalogUse::Effort);
            }
        }
    }

    pub(super) fn menu(&self) -> Vec<(Slash, &'static str, &'static str)> {
        let Some(typed) = self.drafts.composer.text().strip_prefix('/') else {
            return Vec::new();
        };
        if typed.contains(char::is_whitespace) {
            return Vec::new();
        }
        COMMANDS
            .into_iter()
            .filter(|(_, name, _)| name.starts_with(typed))
            .collect()
    }
}
