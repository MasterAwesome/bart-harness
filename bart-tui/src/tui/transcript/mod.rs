use crate::tui::status::Kind as StatusKind;
use crate::tui::text::clip_end;
use crate::tui::text::{self, Clean};
mod answers;
mod block;
mod details;
mod storage;
mod stream;
mod tools;

pub(super) use answers::LookupError as AnswerLookupError;
use block::Kind;
pub(super) use block::{Block, CopySource};
pub(super) use stream::{project, project_rewind};
use tools::{Subject, diff_style, tools};

use std::{
    fmt::Write as _,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

use super::{
    brand, markdown,
    markdown::{RenderRow, RenderText},
    theme,
};
use crate::{
    history::{JobOutcome, Notification, NotificationKind},
    tool::{Diff, Status},
};

const MAX_DETAIL_BYTES: usize = 32 * 1024 * 1024;
const MAX_DETAILS: usize = 1024;
const MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROWS: usize = 32768;

use super::session::UiEvent;

fn append_compaction(detail: &mut Detail, text: &str) {
    const NOTICE: &str = "\n[summary excerpt; earlier output omitted]\n";
    const HEAD: usize = 12 * 1024;
    const TAIL: usize = 32 * 1024;
    const LIMIT: usize = 60 * 1024;
    if !matches!(detail.kind, DetailKind::Compaction { .. }) {
        return;
    }
    let mut output = detail.output.to_string();
    for c in text.chars() {
        if c != '\n' && crate::terminal_text::unsafe_character(c) {
            output.push_str(&c.escape_unicode().to_string());
        } else {
            output.push(c);
        }
        if output.len() >= LIMIT {
            let head = output.floor_char_boundary(HEAD);
            let tail = output.ceil_char_boundary(output.len() - TAIL);
            output.replace_range(head..tail, NOTICE);
        }
    }
    detail.output = Clean::new(&output);
}

struct DisplayDiff {
    path: Clean,
    hunks: Clean,
}

struct Finished {
    output: Arc<Clean>,
    status: Status,
    elapsed: Option<Duration>,
}

struct Call {
    id: String,
    subject: Subject,
    live: Option<u64>,
    started: Instant,
    result: Option<Finished>,
    jobs: Vec<u32>,
    diffs: Vec<DisplayDiff>,
    activities: Option<crate::tool::Activities>,
}

// IDs keep increasing even when old details are removed. They aren't list indexes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DetailSequence(usize);

impl DetailSequence {
    fn take_next(&mut self) -> Self {
        let current = *self;
        self.0 += 1;
        current
    }
}

enum DetailKind {
    UiNotice {
        kind: StatusKind,
        repeats: u16,
        truncated: bool,
        older_omitted: bool,
    },
    Call {
        call_id: String,
    },
    Reasoning,
    Notification {
        terminal_job_id: Option<u32>,
    },
    Note,
    Compaction {
        running: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Highlight {
    pub(super) range: Range<usize>,
    pub(super) style: Style,
}

pub(super) struct Detail {
    kind: DetailKind,
    pub(super) sequence: DetailSequence,
    pub(super) live_run_id: Option<u64>,
    pub(super) order_hint: Option<u64>,
    pub(super) title: Clean,
    pub(super) review: Option<Clean>,
    pub(super) code: Clean,
    pub(super) output: Clean,
    pub(super) highlights: Vec<Highlight>,
}

impl Detail {
    fn refresh_ui_title(&mut self) {
        if let DetailKind::UiNotice {
            repeats,
            older_omitted,
            ..
        } = self.kind
        {
            let prefix = if older_omitted {
                "Older UI notices omitted · "
            } else {
                ""
            };
            self.title = Clean::new(&if repeats == u16::MAX {
                format!("{prefix}UI notice · repeated at least 65535 times")
            } else if repeats > 1 {
                format!("{prefix}UI notice · repeated {repeats} times")
            } else {
                format!("{prefix}UI notice")
            });
        }
    }

    pub(super) fn terminal_job_id(&self) -> Option<u32> {
        match self.kind {
            DetailKind::Notification { terminal_job_id } => terminal_job_id,
            DetailKind::Call { .. }
            | DetailKind::Reasoning
            | DetailKind::Note
            | DetailKind::UiNotice { .. }
            | DetailKind::Compaction { .. } => None,
        }
    }

    pub(super) fn ui_notice_truncated(&self) -> Option<bool> {
        match self.kind {
            DetailKind::UiNotice { truncated, .. } => Some(truncated),
            _ => None,
        }
    }

    pub(super) fn is_compaction(&self) -> bool {
        matches!(self.kind, DetailKind::Compaction { .. })
    }

    pub(super) fn compaction_running(&self) -> bool {
        matches!(self.kind, DetailKind::Compaction { running: true, .. })
    }

    fn is_call(&self, id: &str) -> bool {
        matches!(&self.kind, DetailKind::Call { call_id } if call_id == id)
    }

    fn bytes(&self) -> usize {
        let call_id = match &self.kind {
            DetailKind::Call { call_id } => call_id.capacity(),
            _ => 0,
        };
        size_of::<Self>()
            + call_id
            + self.title.len()
            + self.review.as_ref().map_or(0, |text| text.len())
            + self.code.len()
            + self.output.len()
            + self.highlights.capacity() * size_of::<Highlight>()
    }
}

#[derive(Clone, Copy, Default)]
struct Spacing {
    last: Option<(std::mem::Discriminant<Kind>, u64)>,
    blank: bool,
}

impl Spacing {
    fn render(&mut self, kind: &Kind, mut text: RenderText) -> Option<RenderText> {
        let element = (
            std::mem::discriminant(kind),
            match kind {
                Kind::Prose(id) => *id,
                _ => 0,
            },
        );
        let mut lines = Vec::new();
        for line in text.lines {
            if !matches!(kind, Kind::User(_)) && line.is_blank() {
                self.blank = true;
                continue;
            }
            let distinct = self.last != Some(element)
                || matches!(kind, Kind::User(_) | Kind::Notice) && lines.is_empty();
            if self.last.is_some() && (distinct || self.blank) {
                lines.push(RenderRow::literal(
                    Line::default().style(Style::new().bg(Color::Reset)),
                ));
            }
            self.blank = false;
            self.last = Some(element);
            lines.push(line);
        }
        if lines.is_empty() {
            return None;
        }
        text.lines = lines;
        Some(text)
    }
}

pub(super) struct StoredBlock {
    pub(super) id: u64,
    pub(super) block: Block,
}

#[derive(Clone, Copy, Default)]
enum ReasoningPreview {
    #[default]
    Pending,
    Shown,
}

#[derive(Default)]
pub(super) struct Transcript {
    spacing: Spacing,
    prose: u64,
    reasoning: markdown::Stream,
    reasoning_preview: ReasoningPreview,
    reasoning_detail: Option<(DetailSequence, crate::terminal_text::Display)>,
    text: markdown::Stream,
    responded: bool,
    calls: Vec<Call>,
    details: Vec<Detail>,
    next_detail: DetailSequence,
    ui_notices_omitted: bool,
    compaction: Option<DetailSequence>,
    answers: answers::Answers,
    pastes: Arc<super::paste::Previews>,
    live_tools: Option<Arc<crate::tool::Live>>,
    ui: crate::preferences::Ui,
    blocks: std::collections::VecDeque<StoredBlock>,
    bytes: usize,
    rows: usize,
    next: u64,
    tail: Arc<[RenderRow]>,
    tail_tick: usize,
}

impl Transcript {
    pub(super) fn details(&self) -> &[Detail] {
        &self.details
    }
    pub(super) fn answer(&self, index: usize) -> Result<&str, AnswerLookupError> {
        self.answers.get(index)
    }
    pub(super) fn set_ui(&mut self, ui: crate::preferences::Ui) {
        self.ui = ui;
    }

    pub(super) fn with_pastes(previews: super::paste::Previews) -> Self {
        Self {
            pastes: Arc::new(previews),
            ..Self::default()
        }
    }

    pub(super) fn add_paste_previews(&mut self, previews: super::paste::Previews) {
        if !previews.is_empty() {
            Arc::make_mut(&mut self.pastes).extend(previews);
        }
    }

    pub(super) fn configure_tools(
        &mut self,
        live: Arc<crate::tool::Live>,
        ui: crate::preferences::Ui,
    ) {
        self.live_tools = Some(live);
        self.ui = ui;
    }

    fn tool_rows(&self, width: u16, tick: usize) -> Vec<Line<'static>> {
        if self.ui == crate::preferences::Ui::Cells {
            return tools(&self.calls, self.live_tools.as_deref(), width, tick);
        }
        tools::activities(&self.calls, self.live_tools.as_deref(), width, tick)
    }

    fn emit(&mut self, kind: Kind, text: impl Into<RenderText>, done: &mut Vec<Block>) {
        if let Some(text) = self.spacing.render(&kind, text.into()) {
            match kind {
                Kind::Prose(_) | Kind::Reasoning => {
                    for row in text.lines {
                        let kind = match kind {
                            Kind::Prose(id) => Kind::Prose(id),
                            _ => Kind::Reasoning,
                        };
                        done.push(Block::new(kind, RenderText::from(vec![row])));
                    }
                }
                _ => done.push(Block::new(kind, text)),
            }
        }
    }

    pub(super) fn user(
        &mut self,
        text: &str,
        skills: &[crate::history::Skill],
        steered: bool,
    ) -> Block {
        let text = Clean::new(text);
        let mut rendered = user(&text, steered, &self.pastes);
        for skill in skills {
            rendered.lines.push(RenderRow::literal(Line::styled(
                Clean::new(&format!("  {}", skill.label())),
                muted(Style::new()),
            )));
        }
        let kind = Kind::User(Arc::new(text));
        let rows = self.spacing.render(&kind, rendered).expect("user rows");
        Block::new(kind, rows)
    }

    pub(super) fn resumed(&mut self, journal: &crate::agent::Journal) {
        let (reset, jobs) =
            journal
                .resume_notifications()
                .fold((false, 0), |(reset, jobs), note| {
                    if note.terminal_job().is_some() {
                        (reset, jobs + 1)
                    } else {
                        (true, jobs)
                    }
                });
        if !reset && jobs == 0 {
            return;
        }
        let mut parts = vec!["↻ resumed".to_owned()];
        if jobs > 0 {
            parts.push(format!(
                "{jobs} unresolved job{}",
                if jobs == 1 { "" } else { "s" }
            ));
        }
        if reset {
            parts.push("Python state reset".into());
        }
        parts.push("Ctrl+O details".into());
        let block = self.notice(Text::from(Line::styled(
            Clean::new(&parts.join(" · ")),
            Style::new().dim(),
        )));
        self.append([block], None);
    }

    pub(super) fn notice(&mut self, text: Text<'static>) -> Block {
        Block::new(
            Kind::Notice,
            self.spacing
                .render(&Kind::Notice, text.into())
                .unwrap_or_default(),
        )
    }

    pub(super) fn apply(&mut self, activity: UiEvent, width: u16) -> Vec<Block> {
        let mut done = Vec::new();
        match activity {
            UiEvent::MessageBoundary => self.flush_text(&mut done),
            UiEvent::Review(review) => {
                if let Some(detail) = self
                    .details
                    .iter_mut()
                    .rev()
                    .find(|detail| detail.is_call(review.call_id()))
                {
                    detail.review = Some(Clean::new(&crate::approval::review_line(&review)));
                }
                self.bound_details();
            }
            UiEvent::Compacting => self.start_compaction(),
            UiEvent::CompactionStage(index, total) => {
                if let Some(detail) = self.compaction_detail() {
                    detail.title =
                        Clean::new(&format!("Compaction · generating stage {index}/{total}"));
                    append_compaction(detail, &format!("\n\n--- Stage {index}/{total} ---\n"));
                }
            }
            UiEvent::CompactionText(text) => {
                if let Some(detail) = self.compaction_detail() {
                    append_compaction(detail, &text);
                }
            }
            UiEvent::CompactionStageDone => {
                if let Some(detail) = self.compaction_detail() {
                    detail.title =
                        Clean::new(&detail.title.replace("generating stage", "stage complete"));
                }
            }
            UiEvent::CompactionNoop => {
                self.finish_compaction("no-op · sufficient headroom");
            }
            UiEvent::Compacted(summary) => {
                let had_live = self.compaction.is_some();
                if let Some(detail) = self.compaction_detail() {
                    detail.output = Clean::new(&summary);
                }
                self.finish_compaction("completed");
                if !had_live
                    && !self
                        .details
                        .iter()
                        .any(|detail| detail.is_compaction() && *detail.output == summary)
                {
                    self.compaction_note(&summary);
                }
                self.flush_text(&mut done);
                done.push(self.compaction_block(&summary));
            }
            UiEvent::Reasoning(delta) => self.stream_reasoning(&delta, &mut done),
            UiEvent::Text(delta) => self.stream_text(&delta, &mut done),
            UiEvent::Responded => {
                self.responded = true;
                self.answers.finish();
            }
            UiEvent::Image(path) => {
                let target = crate::tui::links::file(&path);
                self.flush_text(&mut done);
                let path = crate::terminal_text::safe_text(
                    &path.to_string_lossy(),
                    crate::terminal_text::TextMode::Diagnostic,
                );
                let cells = self.ui == crate::preferences::Ui::Cells;
                let title = if cells {
                    format!("view · {path}")
                } else {
                    format!("display({path})")
                };
                let link = crate::tui::links::style(Style::new(), target);
                self.note(&title, &path);
                if link != Style::new()
                    && let Some(detail) = self.details.last_mut()
                {
                    detail.highlights.push(Highlight {
                        range: 0..detail.output.len(),
                        style: link,
                    });
                }
                let quiet = muted(Style::new().italic());
                self.emit(
                    Kind::Tools(Vec::new()),
                    Text::from(Line::from(vec![
                        Span::styled("✓ ", Style::new().fg(theme().success)),
                        Span::styled(
                            if cells { "view" } else { "display" },
                            if cells {
                                quiet.bold()
                            } else {
                                Style::new().fg(brand()).bold()
                            },
                        ),
                        Span::styled(if cells { " · " } else { "(" }, quiet),
                        Span::styled(path, quiet.patch(link)),
                        Span::styled(if cells { "" } else { ")" }, quiet),
                    ])),
                    &mut done,
                );
            }
            UiEvent::ToolStarted {
                id,
                name,
                arguments,
                live,
            } => {
                self.flush_text(&mut done);
                let code = match arguments.get("code").unwrap_or(&serde_json::Value::Null) {
                    serde_json::Value::String(code) => code.clone(),
                    other => other.to_string(),
                };
                let code = Clean::new(&code);
                let subject = Subject::of(&name, &code);
                self.details.push(Detail {
                    sequence: self.next_detail.take_next(),
                    kind: DetailKind::Call {
                        call_id: id.clone(),
                    },
                    live_run_id: live,
                    order_hint: live,
                    title: Clean::new(&format!("{}{}", subject.name, subject.rest)),
                    review: None,
                    code,
                    output: Clean::literal("[running]"),
                    highlights: Vec::new(),
                });
                self.bound_details();
                self.calls.push(Call {
                    id,
                    subject,
                    live,
                    started: Instant::now(),
                    result: None,
                    jobs: Vec::new(),
                    diffs: Vec::new(),
                    activities: None,
                });
            }
            UiEvent::ToolFinished {
                id,
                output,
                status,
                jobs,
                elapsed,
                diffs,
                activities,
            } => done.extend(self.finish_call(
                &id,
                output,
                (status, elapsed),
                (jobs, diffs, activities),
                width,
            )),
            UiEvent::Notification(notification) => self.notify(notification, &mut done, width),
            UiEvent::Steered(input) => {
                self.flush_text(&mut done);
                done.push(self.user(&input.text, &input.skills, true));
            }
            UiEvent::WaitingForReset { message, .. } => {
                self.answers.discard();
                self.flush_text(&mut done);
                done.push(self.notice(Text::from(Clean::new(&format!(
                    "  {message} · waiting for reset · Esc cancels"
                )))));
            }
            UiEvent::RecordingStopped(message) => {
                self.flush_text(&mut done);
                done.push(self.notice(Text::from(Clean::new(&format!("  {message}")))));
            }
            UiEvent::Retrying {
                discarded: true, ..
            } => {
                self.answers.discard();
                self.flush_text(&mut done);
            }
            UiEvent::Retrying {
                discarded: false, ..
            }
            | UiEvent::ToolBytes(_)
            | UiEvent::Approval(_)
            | UiEvent::CompactingBytes(_)
            | UiEvent::RegistryReplaced
            | UiEvent::SkillAttached
            | UiEvent::Started(_)
            | UiEvent::Accounting(_)
            | UiEvent::Limits(_)
            | UiEvent::Renamed(_)
            | UiEvent::Switched(_)
            | UiEvent::Preferred(..)
            | UiEvent::Notice(_)
            | UiEvent::Ended { .. }
            | UiEvent::WindowUnknown
            | UiEvent::Interrupted { .. }
            | UiEvent::Delivering { .. }
            | UiEvent::Messages(_)
            | UiEvent::Rewound(_) => {}
        }
        done
    }

    fn finish_call(
        &mut self,
        id: &str,
        output: Arc<str>,
        (status, elapsed): (Status, Option<Duration>),
        (jobs, diffs, activities): (Vec<u32>, Vec<Diff>, crate::tool::Activities),
        width: u16,
    ) -> Vec<Block> {
        let output = Arc::new(Clean::new(&output));
        let diffs: Vec<_> = diffs
            .into_iter()
            .map(|diff| DisplayDiff {
                path: Clean::new(&crate::terminal_text::safe_text(
                    &diff.path,
                    crate::terminal_text::TextMode::Diff,
                )),
                hunks: Clean::new(&crate::terminal_text::safe_text(
                    &diff.hunks,
                    crate::terminal_text::TextMode::Diff,
                )),
            })
            .collect();
        if let Some(call) = self.calls.iter_mut().find(|call| call.id == id) {
            call.jobs = jobs;
            call.activities = Some(activities);
            if let Some(detail) = self
                .details
                .iter_mut()
                .rev()
                .find(|detail| detail.is_call(id))
            {
                let mut body = output.to_string();
                detail.highlights.clear();
                for diff in &diffs {
                    body.push_str("\n\n");
                    let start = body.len();
                    body.push_str(&diff.path);
                    detail.highlights.push(Highlight {
                        range: start..body.len(),
                        style: crate::tui::links::style(
                            muted(Style::new()).bold(),
                            crate::tui::links::file(std::path::Path::new(&*diff.path)),
                        ),
                    });
                    for line in diff.hunks.lines() {
                        body.push('\n');
                        let start = body.len();
                        body.push_str(line);
                        detail.highlights.push(Highlight {
                            range: start..body.len(),
                            style: diff_style(line).0,
                        });
                    }
                }
                detail.output = Clean::new(&body);
            }
            call.diffs = diffs;
            call.result = Some(Finished {
                output,
                status,
                elapsed,
            });
        }
        self.bound_details();
        if self.calls.iter().all(|call| call.result.is_some()) {
            self.finish_tools(width)
        } else {
            Vec::new()
        }
    }

    fn finish_tools(&mut self, width: u16) -> Vec<Block> {
        let diffs: Vec<_> = self
            .calls
            .iter_mut()
            .flat_map(|call| std::mem::take(&mut call.diffs))
            .collect();
        let text = Text::from(self.tool_rows(width, 0));
        let calls = std::mem::take(&mut self.calls);
        let mut done = Vec::new();
        if let Some(text) = self.spacing.render(&Kind::Tools(Vec::new()), text.into()) {
            done.push(Block::new(Kind::Tools(calls), text));
        }
        if !diffs.is_empty() {
            done.push(Block::new(
                Kind::Diff,
                Text::from(tools::diff_rows(&diffs, "    ")),
            ));
        }
        done
    }

    pub(super) fn track_job(&mut self, job: &crate::tool::LiveSummary) {
        if let Some(detail) = self.details.iter_mut().rev().find(|detail| {
            detail
                .terminal_job_id()
                .is_some_and(|id| job.job == Some(id))
        }) {
            detail.live_run_id = Some(job.id);
            detail.order_hint = Some(job.order);
        }
    }

    fn notify(&mut self, notification: Notification, done: &mut Vec<Block>, width: u16) {
        let job = notification.terminal_job();
        let recovery = matches!(
            notification.kind,
            NotificationKind::Recovery | NotificationKind::JobLost { .. }
        );
        let Notification { text, kind } = notification;
        let text = Clean::new(&text);
        self.flush_text(done);
        let (heading, body) = text.split_once('\n').unwrap_or((&text, ""));
        let heading = heading.trim_start_matches('[').trim_end_matches(']');
        let title = &heading[..heading.floor_char_boundary(heading.len().min(500))];
        if let NotificationKind::JobFinished {
            id,
            outcome,
            command,
            ..
        } = &kind
        {
            let (mark, style, status) = match outcome {
                JobOutcome::Exit(0) => ("✓", Style::new().fg(theme().success), "finished".into()),
                JobOutcome::Exit(code) => (
                    "✗",
                    Style::new().fg(theme().error),
                    format!("failed · exit {code}"),
                ),
                JobOutcome::Signal(signal) => (
                    "✗",
                    Style::new().fg(theme().error),
                    format!("ended · signal {signal}"),
                ),
                JobOutcome::Killed => {
                    ("■", muted(Style::new()), "stopped · effects unknown".into())
                }
                JobOutcome::Unknown => ("?", muted(Style::new()), "ended · status unknown".into()),
            };
            let mut status = format!("job {id} {status}");
            let room = usize::from(width).saturating_sub(text::width(&status) + 5);
            if room > 0 && !command.is_empty() {
                let _ = write!(
                    status,
                    " · {}",
                    clip_end(&command.replace('\n', " ⏎ "), room)
                );
            }
            self.emit(
                Kind::Notice,
                Text::from(Line::from(vec![
                    Span::styled(format!("{mark} "), style),
                    Span::raw(status),
                ])),
                done,
            );
        }
        if matches!(
            kind,
            NotificationKind::JobMatched { .. } | NotificationKind::JobCommandExited { .. }
        ) {
            self.emit(Kind::Notice, Text::from(title.to_owned()), done);
        }
        let order = self
            .details
            .iter()
            .rev()
            .find_map(|detail| detail.order_hint.or(detail.live_run_id));
        self.details.push(Detail {
            sequence: self.next_detail.take_next(),
            kind: DetailKind::Notification {
                terminal_job_id: job,
            },
            live_run_id: None,
            order_hint: order,
            title: if recovery {
                Clean::new(&job.map_or_else(
                    || "Python state reset".into(),
                    |job| format!("resume: job {job} · effects unknown"),
                ))
            } else {
                Clean::new(title)
            },
            review: None,
            code: Clean::new(if recovery {
                ""
            } else if let NotificationKind::JobFinished { command, .. }
            | NotificationKind::JobCommandExited { command, .. } = &kind
            {
                command
            } else {
                heading
                    .split_once(": ")
                    .map_or(heading, |(_, command)| command)
            }),
            output: Clean::new(if recovery { title } else { body }),
            highlights: Vec::new(),
        });
        self.bound_details();
    }

    pub(super) fn end(&mut self, note: Option<&str>, width: u16) -> Vec<Block> {
        let mut done = Vec::new();
        if self.compaction.is_some() {
            if let Some(reason) = note.filter(|note| *note != "interrupted")
                && let Some(detail) = self.compaction_detail()
            {
                let end = reason.floor_char_boundary(reason.len().min(2048));
                append_compaction(detail, "\n\nFailure: ");
                append_compaction(detail, &reason[..end]);
                if end < reason.len() {
                    append_compaction(detail, " [reason truncated]");
                }
            }
            self.finish_compaction(if note == Some("interrupted") {
                "interrupted"
            } else {
                "failed"
            });
        }
        self.answers.finish();
        self.flush_text(&mut done);
        if !self.calls.is_empty() {
            for call in &mut self.calls {
                if call.result.is_none()
                    && let Some(detail) = self
                        .details
                        .iter_mut()
                        .find(|detail| detail.is_call(&call.id))
                {
                    detail.output = Clean::literal("interrupted; effects unknown");
                }
                call.result.get_or_insert_with(|| Finished {
                    output: Arc::new(Clean::literal("interrupted; effects unknown")),
                    status: Status::Failed,
                    elapsed: Some(call.started.elapsed()),
                });
            }
            done.extend(self.finish_tools(width));
        }
        if let Some(note) = note {
            done.push(self.notice(Text::from(Line::styled(
                Clean::new(&format!("  {note}")),
                Style::new().fg(theme().error),
            ))));
        }
        done
    }

    fn start_response(&mut self, done: &mut Vec<Block>) {
        if self.responded {
            self.flush_text(done);
        }
    }

    fn flush_text(&mut self, done: &mut Vec<Block>) {
        self.responded = false;
        self.end_reasoning(done);
        let lines = self.text.finish();
        if !lines.is_empty() {
            self.emit(
                Kind::Prose(self.prose),
                RenderText::from(indented(lines)),
                done,
            );
        }
        self.prose += 1;
        self.spacing.blank = false;
    }

    fn end_reasoning(&mut self, done: &mut Vec<Block>) {
        let lines = self.reasoning.finish();
        let lines = reasoning(&lines, &mut self.reasoning_preview);
        if !lines.is_empty() {
            self.emit(Kind::Reasoning, Text::from(lines), done);
        }
        self.reasoning_preview = ReasoningPreview::Pending;
        self.reasoning_detail = None;
    }

    pub(super) fn stream_reasoning(&mut self, delta: &str, done: &mut Vec<Block>) {
        self.start_response(done);
        if !self.text.is_empty() {
            self.flush_text(done);
        }
        self.project_reasoning(delta);
        self.reasoning.append(delta);
    }

    pub(super) fn stream_text(&mut self, delta: &str, done: &mut Vec<Block>) {
        self.start_response(done);
        if self.reasoning_detail.is_some() {
            self.end_reasoning(done);
        }
        self.answers.append(delta);
        self.text.append(delta);
    }

    pub(super) fn project_reasoning(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let (sequence, filter) = self.reasoning_detail.get_or_insert_with(|| {
            let sequence = self.next_detail.take_next();
            let order = self
                .details
                .last()
                .and_then(|detail| detail.order_hint.or(detail.live_run_id));
            self.details.push(Detail {
                kind: DetailKind::Reasoning,
                sequence,
                live_run_id: None,
                order_hint: order,
                title: Clean::literal("Reasoning text"),
                review: None,
                code: Clean::default(),
                output: Clean::default(),
                highlights: Vec::new(),
            });
            (sequence, crate::terminal_text::Display::default())
        });
        let sequence = *sequence;
        if let Some(detail) = self
            .details
            .iter_mut()
            .find(|detail| detail.sequence == sequence)
        {
            detail.output.append(delta, filter);
        }
        self.bound_details();
    }

    pub(super) fn flush_frame(&mut self) -> Vec<Block> {
        let mut done = Vec::new();
        let lines = self.reasoning.flush();
        let lines = reasoning(lines.iter(), &mut self.reasoning_preview);
        if !lines.is_empty() {
            self.emit(Kind::Reasoning, Text::from(lines), &mut done);
        }
        let lines = self.text.flush();
        if !lines.is_empty() {
            self.emit(
                Kind::Prose(self.prose),
                RenderText::from(indented(lines)),
                &mut done,
            );
        }
        done
    }

    pub(super) fn prepare_tail(&mut self, width: u16, tick: usize) {
        self.tail_tick = tick;
        let tail = self.live(width, tick);
        if self.tail.as_ref() != tail {
            self.tail = tail.into();
        }
    }

    pub(super) fn sync_tail(&mut self, width: u16) {
        self.prepare_tail(width, self.tail_tick);
    }

    fn live(&mut self, width: u16, tick: usize) -> Vec<RenderRow> {
        let mut spacing = self.spacing;
        let mut shown = self.reasoning_preview;
        let mut lines = Vec::new();
        for (element, text) in [
            (
                Kind::Reasoning,
                RenderText::from(Text::from(reasoning(
                    self.reasoning.live().iter(),
                    &mut shown,
                ))),
            ),
            (
                Kind::Prose(self.prose),
                RenderText::from(indented(self.text.live().to_vec())),
            ),
            (
                Kind::Tools(Vec::new()),
                RenderText::from(Text::from(self.tool_rows(width, tick))),
            ),
        ] {
            if let Some(text) = spacing.render(&element, text) {
                lines.extend(text.lines);
            }
        }
        lines
    }
}

fn indented(lines: Vec<RenderRow>) -> Vec<RenderRow> {
    lines
        .into_iter()
        .map(|mut line| {
            line.prefix(Span::raw("  "));
            line
        })
        .collect()
}

fn reasoning<'a>(
    lines: impl IntoIterator<Item = &'a RenderRow>,
    shown: &mut ReasoningPreview,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut emit = |title: &str, heading| {
        if !title.trim().is_empty() && (heading || matches!(*shown, ReasoningPreview::Pending)) {
            out.push(Line::from(format!("◆ {title}")).style(muted(Style::new())));
            *shown = ReasoningPreview::Shown;
        }
    };
    let mut title = String::new();
    for line in lines.into_iter().map(RenderRow::logical_line) {
        let mut heading = true;
        for span in &line.spans {
            let bold = line
                .style
                .patch(span.style)
                .add_modifier
                .contains(Modifier::BOLD);
            for (index, part) in span.content.split('\n').enumerate() {
                if index != 0 {
                    emit(&title, heading);
                    title.clear();
                    heading = true;
                }
                heading &= bold || part.trim().is_empty();
                title.push_str(part);
            }
        }
        emit(&title, heading);
        title.clear();
    }
    out
}

fn user(text: &str, steer: bool, pastes: &super::paste::Previews) -> RenderText {
    let parts = super::paste::display(text, pastes);
    let marker = if steer { "↳ " } else { "› " };
    let mut body = vec![Line::default()];
    for (part, dim) in parts {
        for (index, line) in part.split('\n').enumerate() {
            if index > 0 {
                body.push(Line::default());
            }
            if line.is_empty() {
                continue;
            }
            let style = if dim {
                muted(Style::new())
            } else {
                Style::new()
            };
            body.last_mut()
                .expect("the body starts with a line")
                .spans
                .push(Span::styled(line.to_owned(), style));
        }
    }
    let background = theme().band;
    let accent = Style::new().fg(brand()).bold();
    let style = background.map_or(Style::new().bold(), |color| Style::new().bg(color));
    let mut lines = Vec::new();
    for (index, line) in body.into_iter().enumerate() {
        let mut row = RenderRow::literal(line.style(style));
        let first = if index == 0 {
            Span::styled(marker, accent)
        } else {
            Span::raw("  ")
        };
        row.prefixes(first, Span::raw("  "));
        if background.is_none() {
            row.prefix(Span::styled("▌ ", accent));
        }
        lines.push(row);
    }
    lines.into()
}

pub(super) fn muted(style: Style) -> Style {
    match theme().muted {
        Some(color) => style.fg(color),
        None => style.dim(),
    }
}
