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
        reviews: &std::collections::VecDeque<crate::history::Review>,
        width: u16,
    ) -> Vec<Block> {
        let mut done = Vec::new();
        let message = match entry {
            Entry::Compact(summary) => {
                self.flush_text(&mut done);
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
                        Content::Reasoning(text) => self.stream_reasoning(text, &mut done),
                        Content::Text(text) => {
                            self.flush_text(&mut done);
                            self.stream_text(text, &mut done);
                            self.flush_text(&mut done);
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
                    Arc::make_mut(&mut block.rows).push(RenderRow::literal(Line::styled(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{Assistant, Part, Protocol};
    use std::collections::VecDeque;

    const WIDTH: u16 = 80;
    const EARLIER: &str = "\n    code\n";

    fn feed(transcript: &mut Transcript, events: impl IntoIterator<Item = UiEvent>) {
        for event in events {
            let blocks = transcript.apply(event, WIDTH);
            transcript.append(blocks, None);
        }
    }

    fn live_with_earlier_answer() -> Transcript {
        let mut transcript = Transcript::default();
        feed(
            &mut transcript,
            [UiEvent::Text(EARLIER.into()), UiEvent::Responded],
        );
        transcript
    }

    fn replay(protocol: Protocol, parts: Vec<Content>) -> Transcript {
        let mut transcript = Transcript::default();
        for parts in [vec![Content::Text(EARLIER.into())], parts] {
            let assistant = Message::Assistant(Assistant {
                protocol,
                provider: "fixture".into(),
                model: "fixture".into(),
                parts: parts
                    .into_iter()
                    .map(|content| Part {
                        content,
                        replay: None,
                    })
                    .collect(),
                usage: None,
                continues: false,
                refused: false,
                served_model: None,
            });
            let blocks = transcript.project(Entry::Message(&assistant), &VecDeque::new(), WIDTH);
            transcript.append(blocks, None);
        }
        finish(&mut transcript, None);
        transcript
    }

    fn finish(transcript: &mut Transcript, note: Option<&str>) {
        let blocks = transcript.end(note, WIDTH);
        transcript.append(blocks, None);
    }

    fn assert_answers(transcript: &Transcript, latest: &str) {
        assert_eq!(
            transcript.answer(0).map(str::as_bytes),
            Ok(latest.as_bytes())
        );
        assert_eq!(
            transcript.answer(1).map(str::as_bytes),
            Ok(EARLIER.as_bytes())
        );
        assert_eq!(transcript.answer(2), Err(AnswerLookupError::OutOfRange(2)));
    }

    fn rendered_blocks(transcript: &Transcript) -> Vec<Vec<Line<'static>>> {
        transcript
            .blocks()
            .iter()
            .map(|entry| {
                entry
                    .block
                    .rows
                    .iter()
                    .flat_map(|row| row.project(WIDTH))
                    .collect()
            })
            .collect()
    }

    fn rendered(transcript: &Transcript) -> Vec<String> {
        rendered_blocks(transcript)
            .into_iter()
            .flatten()
            .map(|line| line.to_string().trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect()
    }

    #[test]
    fn retry_discards_copy_draft_without_joining_the_displayed_attempts() {
        let mut live = live_with_earlier_answer();
        feed(&mut live, [UiEvent::Text("half".into())]);
        assert_answers(&live, "half");
        feed(
            &mut live,
            [UiEvent::Retrying {
                message: "connection lost".into(),
                delay: Duration::ZERO,
                discarded: true,
            }],
        );
        assert_eq!(live.answer(0), Ok(EARLIER));
        assert_eq!(live.answer(1), Err(AnswerLookupError::OutOfRange(1)));
        feed(
            &mut live,
            [UiEvent::Text("whole".into()), UiEvent::Responded],
        );
        finish(&mut live, None);
        let replayed = replay(Protocol::Chat, vec![Content::Text("whole".into())]);
        assert_answers(&live, "whole");
        assert_answers(&replayed, "whole");
        assert_eq!(rendered(&live), ["code", "half", "whole"]);
        assert_eq!(rendered(&replayed), ["code", "whole"]);
    }

    #[test]
    fn interleaved_reasoning_only_splits_live_display() {
        let mut live = live_with_earlier_answer();
        feed(
            &mut live,
            [
                UiEvent::Text("A".into()),
                UiEvent::Reasoning("r".into()),
                UiEvent::Text("B".into()),
                UiEvent::Responded,
            ],
        );
        finish(&mut live, None);
        let replayed = replay(
            Protocol::Chat,
            vec![Content::Reasoning("r".into()), Content::Text("AB".into())],
        );
        assert_answers(&live, "AB");
        assert_answers(&replayed, "AB");
        assert_eq!(rendered(&live), ["code", "A", "◆ r", "B"]);
        assert_eq!(rendered(&replayed), ["code", "◆ r", "AB"]);
    }

    #[test]
    fn separate_response_items_render_separately_but_copy_as_one_answer() {
        let mut live = live_with_earlier_answer();
        feed(
            &mut live,
            [
                UiEvent::Text("First.".into()),
                UiEvent::MessageBoundary,
                UiEvent::Text("# Next".into()),
                UiEvent::Responded,
            ],
        );
        finish(&mut live, None);
        let replayed = replay(
            Protocol::Responses,
            vec![
                Content::Text("First.".into()),
                Content::Text("# Next".into()),
            ],
        );
        for transcript in [&live, &replayed] {
            assert_answers(transcript, "First.# Next");
            assert_eq!(rendered(transcript), ["code", "First.", "Next"]);
        }
        assert_eq!(rendered_blocks(&live), rendered_blocks(&replayed));
    }

    #[test]
    fn interrupted_response_retains_its_partial_markdown_as_one_answer() {
        let mut live = live_with_earlier_answer();
        feed(
            &mut live,
            [
                UiEvent::Text("partial ".into()),
                UiEvent::MessageBoundary,
                UiEvent::Text("answer\n".into()),
            ],
        );
        finish(&mut live, Some("interrupted"));
        feed(
            &mut live,
            [UiEvent::Text("next answer".into()), UiEvent::Responded],
        );
        assert_eq!(live.answer(0), Ok("next answer"));
        assert_eq!(live.answer(1), Ok("partial answer\n"));
        assert_eq!(live.answer(2), Ok(EARLIER));
        assert_eq!(live.answer(3), Err(AnswerLookupError::OutOfRange(3)));
    }
}
