use super::*;

impl App {
    pub(super) fn agent_event(
        &mut self,
        update: UiEvent,
        width: u16,
    ) -> Vec<crate::tui::transcript::Block> {
        let mut done = Vec::new();
        let applied_change = if matches!(update, UiEvent::Preferred(..)) {
            self.work.preferences.take().map(|(_, change)| change)
        } else {
            None
        };

        match update {
            UiEvent::RegistryReplaced => self.effects.push(Effect::RebindRegistry),
            UiEvent::Accounting(accounting) => self.info.accounting = accounting,
            UiEvent::Limits(limits) => self.info.limits = Some(limits),
            UiEvent::Preferred(ticket, Ok((preferences, warning))) => {
                let context = &self.session.as_ref().expect("active session").context;
                self.info.confinement = context.confinement();
                self.info.mcp_retry = if context.mcp_pending() {
                    applied_change
                        .filter(|change| {
                            matches!(
                                change,
                                Change::Sandbox(_)
                                    | Change::Network(_)
                                    | Change::Approval(_)
                                    | Change::Reviewer(_)
                            )
                        })
                        .or_else(|| self.info.mcp_retry.clone())
                        .or(Some(Change::Sandbox(
                            preferences.effective.permissions.sandbox.enabled,
                        )))
                } else {
                    None
                };
                let (prompted, unsaved_edit) = match &mut self.surface {
                    Some(Surface::Picker(picker)) => picker.preferences_saved(
                        &preferences.effective,
                        self.info.mcp_retry.clone(),
                        ticket,
                    ),
                    _ => (false, false),
                };
                self.transcript.set_ui(preferences.effective.ui);
                self.info.preferences = *preferences;
                if prompted {
                    self.set_picker(None);
                }
                self.status.clear();
                if warning.is_some() || unsaved_edit {
                    let mut notice = warning.unwrap_or_default();
                    if self.info.mcp_retry.is_some() && !notice.is_empty() {
                        notice.push_str("; retry via MCP restart in /settings");
                    }
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
                if self.info.settings.provider != settings.provider
                    || self.info.settings.model != settings.model
                {
                    self.info.preferences.effective.service_tier =
                        crate::preferences::ServiceTier::Auto;
                }
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
            UiEvent::Started(inputs) => {
                for input in &inputs {
                    if let Some(at) = self.drafts.steers.iter().position(|p| &p.input == input) {
                        self.drafts.steers.remove(at);
                    }
                }
                self.turn = Some(Turn::new());
            }
            UiEvent::Renamed(result) => match result {
                Ok(name) => self.info.name = Some(Clean::new(&name)),
                Err(error) => self.set_status(StatusKind::Error, format!("cannot rename: {error}")),
            },
            UiEvent::Ended {
                error,
                recover_input,
                rejected,
                ack,
            } => {
                if matches!(self.surface, Some(Surface::Approval(_))) {
                    self.surface = None;
                }
                self.turn = None;
                let error = error.map(|error| format!("bart: {error}"));
                done.extend(self.transcript.end(error.as_deref(), width));
                self.status.clear();
                let next = if recover_input {
                    self.cancel_task(Kind::Prepare);
                    InputBoundary::Recover(rejected)
                } else {
                    InputBoundary::Advance
                };
                self.effects.push(Effect::InputBoundary { next, ack });
            }
            UiEvent::WindowUnknown => {
                self.set_picker(Some(Picker::window(
                    self.info.preferences.effective.clone(),
                    self.info.mcp_retry.clone(),
                )));
                self.load_context();
            }
            UiEvent::Interrupted {
                resend,
                rejected,
                ack,
            } => {
                if matches!(self.surface, Some(Surface::Approval(_))) {
                    self.surface = None;
                }
                self.turn = None;
                self.status.clear();
                done.extend(self.transcript.end(Some("interrupted"), width));
                let next = if resend {
                    if !rejected.is_empty() {
                        self.restore_steers(rejected);
                    }
                    InputBoundary::Resend
                } else {
                    self.cancel_task(Kind::Prepare);
                    InputBoundary::Recover(rejected)
                };
                self.effects.push(Effect::InputBoundary { next, ack });
            }
            UiEvent::Delivering { text, skills } => {
                done.push(self.transcript.user(&text, &skills, false));
            }
            UiEvent::Messages(messages) if messages.is_empty() => {
                self.set_status(StatusKind::Info, "no message to rewind to")
            }
            UiEvent::Messages(messages) => self.set_picker(Some(Picker::messages(messages))),
            UiEvent::Rewound(Ok((text, transcript, accounting))) => {
                self.surface = None;
                self.info.accounting = accounting;
                self.replace_transcript(*transcript);
                self.cancel_task(Kind::Image);
                if self.drafts.composer.is_empty() {
                    self.restore_draft(&text);
                }
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
