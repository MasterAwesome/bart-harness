use super::*;

impl App {
    pub(super) fn agent_event(
        &mut self,
        update: UiEvent,
        width: u16,
    ) -> Vec<crate::tui::transcript::Block> {
        let mut done = Vec::new();
        if matches!(update, UiEvent::Preferred(..)) {
            self.work.preferences = None;
        }

        match update {
            UiEvent::RegistryReplaced => self.effects.push(Effect::RebindRegistry),
            UiEvent::Accounting(accounting) => self.info.accounting = accounting,
            UiEvent::Limits(limits) => self.info.limits = Some(limits),
            UiEvent::Preferred(ticket, Ok((preferences, warning))) => {
                let (prompted, unsaved_edit) = match &mut self.surface {
                    Some(Surface::Picker(picker)) => {
                        picker.preferences_saved(&preferences.effective, ticket)
                    }
                    _ => (false, false),
                };
                self.info.confinement = self
                    .session
                    .as_ref()
                    .expect("active session")
                    .context
                    .confinement();
                self.transcript.set_ui(preferences.effective.ui);
                self.info.preferences = *preferences;
                if prompted {
                    self.set_picker(None);
                }
                self.status.clear();
                if warning.is_some() || unsaved_edit {
                    let mut notice = warning.unwrap_or_default();
                    if unsaved_edit {
                        if !notice.is_empty() {
                            notice.push_str("; ");
                        }
                        notice.push_str("current context edit is unsaved");
                    }
                    self.set_status(StatusKind::Warning, notice);
                } else if prompted {
                    self.set_status(
                        StatusKind::Info,
                        "context window saved; send your message again",
                    );
                }
            }
            UiEvent::Preferred(_, Err(error)) => {
                if let Some(Surface::Picker(picker)) = &mut self.surface {
                    picker.preference_failed();
                }
                self.set_status(StatusKind::Error, error);
            }
            UiEvent::Switched(Ok(settings)) => {
                self.info.settings = settings;
                self.info.limits = None;
            }
            UiEvent::Switched(Err(error)) | UiEvent::Rewound(Err(error)) => {
                self.set_status(StatusKind::Error, error)
            }
            UiEvent::Notice(message) => {
                done.extend(self.transcript.apply(UiEvent::MessageBoundary, width));
                done.push(
                    self.transcript
                        .notice(Text::from(Clean::new(&format!("  {message}")))),
                );
            }
            UiEvent::Started => {
                self.turn = Some(Turn::new());
            }
            UiEvent::Renamed(result) => match result {
                Ok(name) => self.info.name = Some(Clean::new(&name)),
                Err(error) => self.set_status(StatusKind::Error, format!("cannot rename: {error}")),
            },
            UiEvent::Ended(error) => {
                if matches!(self.surface, Some(Surface::Approval(_))) {
                    self.surface = None;
                }
                self.turn = None;
                let error = error.map(|error| format!("bart: {error}"));
                done.extend(self.transcript.end(error.as_deref(), width));
                self.status.clear();
                if error.is_some() {
                    self.reclaim_steers();
                }
                if !self.drafts.queued.is_empty() {
                    let input = self.drafts.queued.remove(0).input;

                    done.push(self.submit(input));
                }
            }
            UiEvent::WindowUnknown => {
                self.set_picker(Some(Picker::window(
                    self.info.preferences.effective.clone(),
                )));
                self.load_context();
            }
            UiEvent::Interrupted => {
                if matches!(self.surface, Some(Surface::Approval(_))) {
                    self.surface = None;
                }
                self.turn = None;
                self.status.clear();
                done.extend(self.transcript.end(Some("interrupted"), width));
            }
            UiEvent::Resent(input) => {
                self.drafts.steers.clear();

                done.push(self.transcript.user(&input.text, &input.skills, false));
            }
            UiEvent::Messages(messages) if messages.is_empty() => {
                self.set_status(StatusKind::Info, "no message to rewind to")
            }
            UiEvent::Messages(messages) => self.set_picker(Some(Picker::messages(messages))),
            UiEvent::Rewound(Ok((text, transcript, accounting))) => {
                self.surface = None;
                self.info.accounting = accounting;
                self.transcript = *transcript;
                self.scrollback = Default::default();
                done.push(self.rewound(text));
            }
            event => done.extend(self.activity(event, width)),
        }
        done
    }

    fn activity(&mut self, activity: UiEvent, width: u16) -> Vec<crate::tui::transcript::Block> {
        let mut done = Vec::new();

        self.info.recording_stopped |= matches!(activity, UiEvent::RecordingStopped(_));
        match &activity {
            UiEvent::CompactionNoop => {
                self.set_status(StatusKind::Info, "Context already has sufficient headroom.")
            }
            UiEvent::WaitingForReset { message, .. } => self.set_status(
                StatusKind::Info,
                format!("{message} · waiting for reset · Esc cancels"),
            ),
            UiEvent::Retrying {
                message,
                delay,
                discarded,
            } => self.status.set(
                StatusKind::Warning,
                Clean::new(&format!(
                    "{}retrying in {:.1}s · {message}",
                    if *discarded {
                        "response discarded · "
                    } else {
                        ""
                    },
                    delay.as_secs_f64()
                )),
                Some(*delay),
            ),
            UiEvent::RecordingStopped(message) => self.set_status(StatusKind::Warning, message),
            _ => {}
        }

        if let UiEvent::Approval(answer) = &activity {
            self.set_picker(None);
            self.surface = None;
            self.set_approval(answer.clone());
        }
        if let Some(turn) = &mut self.turn {
            turn.progress.activity(&activity);
        }
        if let UiEvent::Steered(text) = &activity
            && let Some(position) = self
                .drafts
                .steers
                .iter()
                .position(|steer| &steer.input == text)
        {
            self.drafts.steers.remove(position);
        }
        let job = match &activity {
            UiEvent::Notification(note) => note.terminal_job().and_then(|id| {
                self.session
                    .as_ref()
                    .expect("active session")
                    .live
                    .jobs()
                    .into_iter()
                    .find(|job| job.job == Some(id))
            }),
            _ => None,
        };
        done.extend(self.transcript.apply(activity, width));
        if let Some(job) = job {
            self.transcript.track_job(&job);
        }
        done
    }
}
