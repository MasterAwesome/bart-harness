use super::*;

impl App {
    pub(super) fn prepare_or_commit(&mut self, delivery: Delivery) {
        if !self.drafts.composer.has_meaningful_input() {
            return;
        }
        if !self.draft_fits(None) {
            self.set_status(StatusKind::Info, INPUT_FULL);
            return;
        }
        if self.drafts.composer.needs_save()
            || self.drafts.composer.text().starts_with('/')
            || !paste::references(self.drafts.composer.text()).is_empty()
        {
            return self.prepare_input(delivery);
        }
        self.commit_prepared_input(
            PreparedInput {
                text: self.drafts.composer.text_with_references(),
                skills: Vec::new(),
            },
            delivery,
        )
    }

    fn prepare_input(&mut self, delivery: Delivery) {
        if self.active_task(Kind::Prepare) {
            self.set_status(StatusKind::Info, "input is still being prepared");
            return;
        }
        if let Some(session) = &self.session {
            self.start_task(Task::Prepare {
                composer: self.drafts.composer.clone(),
                artifacts: session.context.artifacts(),
                delivery,
            });
        }
    }

    pub(super) fn input_prepared(&mut self, preparation: Preparation) {
        if self.drafts.composer.text() != preparation.draft {
            self.set_status(
                StatusKind::Info,
                "draft changed while preparing; press Enter to send",
            );
            return;
        }
        if matches!(&preparation.result, Ok(prepared) if prepared.text.trim().is_empty()) {
            return;
        }
        self.transcript.add_paste_previews(preparation.previews);
        self.drafts.composer = preparation.composer;
        match preparation.result {
            Ok(prepared)
                if !matches!(self.surface, Some(Surface::Picker(_)))
                    && !matches!(self.surface, Some(Surface::Approval(_))) =>
            {
                self.commit_prepared_input(prepared, preparation.delivery);
            }
            Ok(_) => self.set_status(StatusKind::Info, "input ready; press Enter to send"),
            Err(error) => self.set_status(StatusKind::Error, error),
        }
    }

    pub(super) fn commit_prepared_input(&mut self, prepared: PreparedInput, delivery: Delivery) {
        if prepared.text.trim().is_empty() {
            return;
        }
        let mut input = Input {
            text: prepared.text,
            images: self.drafts.images.clone(),
            pasted_bytes: self.drafts.composer.pasted_bytes(),
            skills: prepared.skills,
        };
        if !inputs_fit(
            self.drafts
                .queued
                .iter()
                .chain(&self.drafts.steers)
                .map(|pending| &pending.input)
                .chain([&input]),
            InputSize::default(),
        ) {
            self.set_status(StatusKind::Info, INPUT_FULL);
            return;
        }
        self.status.clear();
        self.cancel_task(Kind::Image);
        self.drafts.composer.clear();
        self.drafts.browsing = None;
        self.remember_input(input.text.clone());
        self.drafts.images.clear();
        input
            .images
            .retain(|image| input.text.contains(&image.label));
        match delivery {
            Delivery::Queue if self.turn.is_some() => {
                self.drafts.queued.push(PendingInput::new(input));
            }
            Delivery::Send if self.turn.is_some() => self.effects.push(Effect::Steer(input)),
            Delivery::Send | Delivery::Queue => self.submit_input(input),
        }
    }

    pub(super) fn submit_input(&mut self, input: Input) {
        let text = self.submit(input);
        self.append_display([text]);
    }

    pub(super) fn submit(&mut self, input: Input) -> crate::tui::transcript::Block {
        self.cancel_picker_tasks();
        let text = self.transcript.user(&input.text, &input.skills, false);
        self.send_command(Command::Submit(input));
        self.turn = Some(Turn::new());
        text
    }

    pub(super) fn interrupt(&mut self, resend: bool) {
        if !resend {
            self.reclaim_steers();
        }
        self.effects.push(Effect::Interrupt { resend });
    }

    pub(super) fn reclaim_steers(&mut self) {
        self.effects.push(Effect::Reclaim);
    }

    pub(super) fn restore_steers(&mut self, unsent: Vec<Input>) {
        self.drafts.steers.clear();

        if unsent.is_empty() {
            return;
        }
        let text_bytes = unsent
            .iter()
            .map(|input| paste::content_bytes(&input.text))
            .sum::<usize>()
            + unsent.len()
            - 1
            + self.drafts.composer.content_bytes()
            + usize::from(!self.drafts.composer.is_empty());
        if text_bytes <= MAX_DRAFT_BYTES {
            let merged = merge(unsent.clone(), "\n");
            let append = self.drafts.composer.clone();
            if self.restore_draft(&merged.text) {
                self.drafts.images.extend(merged.images);
                if !append.is_empty() {
                    self.drafts.composer.insert("\n");
                    self.drafts.composer.append(append);
                }
                return;
            }
        } else {
            self.set_status(
                StatusKind::Info,
                "refused steers returned to the queue; together they exceed the 1 MiB draft limit",
            );
        }
        self.drafts
            .queued
            .splice(0..0, unsent.into_iter().map(PendingInput::new));
    }

    pub(super) fn restore_draft(&mut self, text: &str) -> bool {
        let result = if let Some(artifacts) = &self.drafts.pastes {
            Composer::restore(text, artifacts)
        } else {
            let mut composer = Composer::default();
            if composer.set(text.to_owned()) {
                Ok(composer)
            } else {
                Err(io::Error::other(
                    "draft limit is 1 MiB; draft replacement refused",
                ))
            }
        };
        match result {
            Ok(composer) => {
                self.cancel_task(Kind::Prepare);
                self.cancel_task(Kind::Image);
                self.transcript
                    .add_paste_previews(composer.paste_previews());
                self.drafts.composer = composer;
                true
            }
            Err(error) => {
                self.set_status(StatusKind::Error, format!("cannot restore draft: {error}"));
                false
            }
        }
    }

    pub(super) fn recall(&mut self, older: bool) {
        let index = match (&self.drafts.browsing, older) {
            (None, true) if !self.drafts.sent.is_empty() => self.drafts.sent.len() - 1,
            (Some((index, _)), true) => index.saturating_sub(1),
            (Some((index, _)), false) if index + 1 < self.drafts.sent.len() => index + 1,
            (Some(_), false) => {
                let (_, draft) = self.drafts.browsing.take().expect("browsing");
                self.drafts.composer = draft;
                self.cancel_task(Kind::Image);
                return;
            }
            _ => return,
        };
        let draft = match self.drafts.browsing.take() {
            Some((_, draft)) => draft,
            None => self.drafts.composer.clone(),
        };
        let text = self.drafts.sent[index].clone();
        self.restore_draft(&text);
        self.drafts.browsing = Some((index, draft));
    }

    pub(super) fn paste_visible(&mut self, text: &str) {
        self.clear_picker_feedback();
        if let Some(Surface::Picker(picker)) = &mut self.surface {
            if let Err(reason) = picker.paste(text) {
                self.set_status(StatusKind::Info, reason);
            }
        } else if matches!(self.surface, Some(Surface::Approval(_))) {
            self.set_status(StatusKind::Info, "paste refused during approval");
        } else {
            self.paste(text);
        }
    }

    fn paste(&mut self, text: &str) {
        let folds = paste::folds(&paste::normalize(text));
        if folds && self.drafts.next_paste == usize::MAX {
            self.set_status(StatusKind::Info, "paste numbering exhausted; paste refused");
            return;
        }
        let draft = self.drafts.composer.clone();
        if !self.drafts.composer.paste(text, self.drafts.next_paste) {
            self.set_status(
                StatusKind::Info,
                "paste or draft exceeds 1 MiB; paste refused",
            );
        } else if !self.draft_fits(None) {
            self.drafts.composer = draft;
            self.set_status(StatusKind::Info, INPUT_FULL);
        } else if folds {
            self.drafts.next_paste += 1;
        }
    }

    pub(super) fn attach(&mut self, mut image: InlineImage) {
        image.label = format!("[image {}]", self.drafts.next_image);
        let draft = self.drafts.composer.clone();
        if !self.drafts.composer.insert(&image.label) {
            self.set_status(StatusKind::Info, "draft limit is 1 MiB; attachment refused");
        } else if !self.draft_fits(Some(&image)) {
            self.drafts.composer = draft;
            self.set_status(StatusKind::Info, INPUT_FULL);
        } else {
            self.drafts.next_image += 1;
            self.drafts.images.push(image);
        }
    }

    pub(super) fn paste_image(&mut self) {
        if self.surface.is_some() {
            self.set_status(
                StatusKind::Info,
                "clipboard image refused: composer is not visible",
            );
            return;
        }
        if self.active_task(Kind::Image) {
            return;
        }
        self.start_task(Task::Image);
    }

    pub(super) fn insert(&mut self, text: &str) {
        if !self.drafts.composer.insert(text) {
            self.set_status(StatusKind::Info, "draft limit is 1 MiB; insertion refused");
        }
    }

    fn draft_fits(&self, extra: Option<&InlineImage>) -> bool {
        inputs_fit(
            self.drafts
                .queued
                .iter()
                .chain(&self.drafts.steers)
                .map(|pending| &pending.input),
            InputSize::draft(
                self.drafts.composer.text(),
                self.drafts.images.iter().chain(extra),
            )
            .with_pastes(self.drafts.composer.pasted_bytes()),
        )
    }

    pub(super) fn remember_input(&mut self, text: String) {
        if text.trim().is_empty() || self.drafts.sent.last() == Some(&text) {
            return;
        }
        self.drafts.sent.push(text);
        if self.drafts.sent.len() > 64 {
            self.drafts.sent.remove(0);
        }
    }

    pub(super) fn rewound(&mut self, text: String) -> crate::tui::transcript::Block {
        self.cancel_task(Kind::Image);
        let mut lines = vec![Line::from(
            "  rewound: conversation removed; file and external effects remain",
        )];
        lines.extend(
            Clean::new(&text)
                .lines()
                .map(|line| Line::from(format!("  {line}"))),
        );
        let notice = self.transcript.notice(Text::from(lines));
        if self.drafts.composer.is_empty() {
            self.restore_draft(&text);
        }
        notice
    }

    pub(super) fn mentions(&self) -> Vec<crate::tui::mention::Found> {
        match (self.drafts.composer.mention(), &self.drafts.files) {
            (Some(query), Some(files))
                if !self.drafts.mention_closed
                    && !matches!(self.surface, Some(Surface::Picker(_))) =>
            {
                files.matches(query)
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn wants_files(&mut self) -> bool {
        if self.drafts.composer.mention().is_none() {
            self.drafts.files = None;
            self.drafts.mention_closed = false;
            return false;
        }
        self.drafts.files.is_none()
    }

    pub(super) fn files_indexed(&mut self, files: crate::tui::mention::Index) {
        if files.truncated {
            self.set_status(
                StatusKind::Info,
                "file suggestions cover the first 200,000 files",
            );
        }
        if self.drafts.composer.mention().is_some() {
            self.drafts.files = Some(files);
        }
    }
}
