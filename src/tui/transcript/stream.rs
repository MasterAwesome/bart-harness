use super::*;
use crate::{
    agent::Session,
    history::{Content, Entry, Message},
};

pub(in crate::tui) fn project(transcript: &mut Transcript, session: &Session, width: u16) {
    for entry in session.journal().entries() {
        let blocks = transcript.project(entry, session.journal().reviews(), width);
        transcript.append(blocks, None);
    }
    let end = transcript.end(None, width);
    transcript.append(end, None);
}

pub(in crate::tui) async fn project_rewind(session: &Session) -> Transcript {
    let artifacts = session.tool_context().artifacts();
    let previews =
        tokio::task::spawn_blocking(move || super::super::paste::previews(artifacts.dir()))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
    let mut transcript = Transcript::with_pastes(previews);
    transcript.configure_tools(session.live(), session.preferences().effective.ui);
    for entry in session.journal().entries() {
        let blocks = transcript.project(entry, session.journal().reviews(), 120);
        transcript.append(blocks, None);
        tokio::task::yield_now().await;
    }
    let end = transcript.end(None, 120);
    transcript.append(end, None);
    transcript
}

impl Transcript {
    fn project(
        &mut self,
        entry: Entry<'_>,
        reviews: &std::collections::VecDeque<crate::approval::Review>,
        width: u16,
    ) -> Vec<Block> {
        let mut done = Vec::new();
        let message = match entry {
            Entry::Compact(summary) => {
                self.flush_text(&mut done, width);
                self.compaction_note(summary);
                done.push(self.compaction_block(summary));
                return done;
            }
            Entry::Message(message) => message,
        };
        match message {
            Message::User(text) => {
                done.extend(self.end(None, width));
                done.push(self.user(text, &[], false));
            }
            Message::Assistant(assistant) => {
                for part in &assistant.parts {
                    match &part.content {
                        Content::Reasoning(text) => self.stream_reasoning(text, &mut done, width),
                        Content::Text(text) => {
                            self.flush_text(&mut done, width);
                            self.stream_text(text, &mut done, width);
                            self.flush_text(&mut done, width);
                        }
                        Content::ToolCall(call) => {
                            done.extend(self.apply(
                                UiEvent::ToolStarted {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                    live: None,
                                },
                                width,
                            ));
                            if let Some(review) = reviews
                                .iter()
                                .rev()
                                .find(|review| review.call_id() == call.id)
                            {
                                done.extend(self.apply(UiEvent::Review(review.clone()), width));
                            }
                        }
                        Content::Other => {}
                    }
                }
                done.extend(self.apply(UiEvent::Responded, width));
            }
            Message::ToolResult(result) => done.extend(self.apply(
                UiEvent::ToolFinished {
                    id: result.call_id.clone(),
                    output: result.output.clone(),
                    status: if result.is_error {
                        Status::Failed
                    } else {
                        Status::Succeeded
                    },
                    jobs: result.jobs.clone(),
                    diffs: result.diffs.clone(),
                    activities: result.activities.clone(),
                    elapsed: None,
                },
                width,
            )),
            Message::Notification(note) => {
                done.extend(self.apply(UiEvent::Notification(note.clone()), width))
            }
            Message::Image(image) => {
                if !matches!(
                    self.blocks.back().map(|entry| &entry.block.kind),
                    Some(Kind::User(_))
                ) {
                    done.extend(self.apply(UiEvent::Image(image.path.clone()), width));
                }
            }
            Message::Instruction(text) => {
                self.note("Archived context", text);
                done.push(self.notice(Text::from(Clean::new(text))));
            }
            Message::Skill(skill) => {
                if let Some(
                    block @ Block {
                        kind: Kind::User(_),
                        ..
                    },
                ) = self.blocks.back_mut().map(|entry| &mut entry.block)
                {
                    let before = block.bytes();
                    Arc::make_mut(&mut block.rows).push(RenderRow::Literal(Line::styled(
                        Clean::new(&format!("  {}", skill.label())),
                        muted(Style::new()),
                    )));
                    self.bytes += block.bytes() - before;
                    self.rows += 1;
                }
            }
            Message::UserSource(_) => {}
        }
        done
    }
    pub(super) fn compaction_block(&mut self, summary: &str) -> Block {
        let text = self
            .spacing
            .render(
                &Kind::Notice,
                Text::from(Line::styled(
                    "  Context compacted · Ctrl+O summary",
                    Style::new().dim(),
                ))
                .into(),
            )
            .unwrap_or_default();
        Block::new(Kind::Compaction(Arc::new(Clean::new(summary))), text)
    }
}
