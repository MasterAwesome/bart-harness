use std::{fmt::Write as _, time::Duration};

use similar::TextDiff;

use super::{Refusal, host::End};
use crate::{
    history::InlineImage,
    tool::{Diff, Output, Run, Status, output::Collector},
};

const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

pub(super) struct Earlier {
    pub(super) cell: u64,
    pub(super) output: Box<Output>,
    pub(super) continued: bool,
}

pub(super) struct EarlierNotes {
    pub(super) cell: u64,
    pub(super) notes: String,
}

#[derive(Clone, Copy)]
pub(super) enum Participation {
    Started,
    Waited,
}

pub(super) struct Capture {
    pub(super) cell: u64,
    pub(super) continued: bool,
    pub(super) collector: Collector,
    pub(super) effects: Effects,
    pub(super) stream: crate::tool::live::Stream,
    pub(super) cancelled: bool,
}

impl Capture {
    pub(super) fn new(collector: Collector, effects: Effects, live: &Run) -> Self {
        Self {
            cell: collector.cell().expect("Python cell collector"),
            continued: false,
            collector,
            effects,
            stream: live.progress().stream(),
            cancelled: false,
        }
    }
    pub(super) fn continuation(collector: Collector, effects: Effects, live: &Run) -> Self {
        let mut capture = Self {
            continued: true,
            ..Self::new(collector, effects, live)
        };
        capture
            .collector
            .line(&bart!(Cell, "cell[{}] output continues", capture.cell));
        capture
    }

    pub(super) async fn finish(
        mut self,
        end: End,
        timeout: Duration,
        participation: Participation,
        note: Option<String>,
        prior: Option<EarlierNotes>,
    ) -> Output {
        tokio::task::spawn_blocking(move || {
            self.stream.finish(|line| self.collector.line(line));
            let cell = self.cell;
            let (lifecycle, status) = result_notes(
                end,
                timeout,
                &self.effects.changed,
                note.as_deref(),
                cell,
                participation,
            );
            let (mut text, delivery, saved) = self.collector.finish();
            // Trim trailing newlines only when all output fits. Otherwise the
            // displayed byte count must still match the text we kept.
            if delivery.is_empty() {
                text.truncate(text.trim_end_matches('\n').len());
            }
            let (notice, summaries) = self.effects.process_summaries.finish(&text);
            let mut notes = lifecycle;
            for summary in &summaries {
                separate(&mut notes);
                notes.push_str(summary);
            }
            if !notice.is_empty() {
                separate(&mut notes);
                notes.push_str(&notice);
            }

            if !delivery.is_empty() {
                separate(&mut notes);
                notes.push_str(&delivery);
            }
            notes.truncate(notes.trim_end_matches('\n').len());
            if let Some(EarlierNotes {
                cell: earlier,
                notes: prior,
            }) = prior.filter(|prior| !prior.notes.is_empty())
            {
                let current = std::mem::take(&mut notes);
                notes = format!(
                    "{}\n{}",
                    bart!(Cell, "cell[{earlier}] notes"),
                    prior.trim_matches('\n')
                );
                if !current.is_empty() {
                    separate(&mut notes);
                    notes.push_str(&bart!(Cell, "cell[{cell}] notes"));
                    separate(&mut notes);
                    notes.push_str(&current);
                }
            }
            let prefix_len = Some(text.len());
            let text = crate::tool::output::render(&text, &notes);
            Output {
                notes,
                prefix_len,
                text,
                status,
                jobs: self.effects.jobs,
                images: self.effects.images,
                saved: Some(saved),
                cell: Some(cell),
                diffs: self.effects.diffs,
                activities: crate::tool::Activities::default(),
            }
        })
        .await
        .expect("capture worker")
    }
}

pub(super) fn result_notes(
    end: End,
    timeout: Duration,
    changed: &[String],
    note: Option<&str>,
    cell: u64,
    participation: Participation,
) -> (String, Status) {
    let mut notes = String::new();
    let status = conclude(&mut notes, end, timeout, participation, cell);
    if status == Status::Failed && !changed.is_empty() {
        separate(&mut notes);
        let _ = write!(
            notes,
            "{}",
            bart!(
                Cell,
                "cell[{cell}] changes made before the failure: {}",
                changed.join("; ")
            )
        );
    }
    if let Some(note) = note {
        let note = bart!(Cell, "cell[{cell}] {note}");
        notes = if notes.is_empty() {
            note.to_owned()
        } else {
            format!("{note}\n{notes}")
        };
    }
    (notes, status)
}

impl Earlier {
    pub(super) fn prefix(&self) -> String {
        let prefix = &self.output.text[..self.output.prefix_len.unwrap_or(self.output.text.len())];
        if self.continued {
            prefix.to_owned()
        } else if prefix.is_empty() {
            bart!(Cell, "cell[{}] result", self.cell)
        } else {
            format!("{}\n{prefix}", bart!(Cell, "cell[{}] result", self.cell))
        }
    }
}

pub(super) fn denied_prior(refusal: Refusal) -> Output {
    let (earlier, reason) = match refusal {
        Refusal::Unsettled(earlier) => (earlier, None),
        Refusal::Denied {
            earlier: Some(earlier),
            reason,
        } => (earlier, Some(reason)),
        Refusal::Denied {
            earlier: None,
            reason,
        } => return Output::error(reason),
    };
    let prefix = earlier.prefix();
    let mut prior = *earlier.output;
    prior.cell = None;
    if let Some(reason) = reason {
        separate(&mut prior.notes);
        prior.notes.push_str(&reason);
    }
    prior.prefix_len = Some(prefix.len());
    prior.text = crate::tool::output::render(&prefix, &prior.notes);
    prior.status = Status::Failed;
    prior
}

pub(super) fn conclude(
    text: &mut String,
    end: End,
    timeout: Duration,
    participation: Participation,
    cell: u64,
) -> Status {
    let earlier = format!("cell[{cell}]");
    match end {
        End::Done {
            failed,
            interrupted,
        } => {
            if interrupted {
                separate(text);
                let _ = write!(
                    text,
                    "{}",
                    bart!(
                        Cell,
                        "{earlier} timed out after {}s and was interrupted; Python state kept",
                        timeout.as_secs()
                    )
                );
            }
            if failed {
                Status::Failed
            } else {
                Status::Succeeded
            }
        }
        End::Cancelled => {
            separate(text);
            text.push_str(&bart!(
                Cell,
                "{earlier} cancelled; Python state kept, effects so far remain"
            ));
            Status::Failed
        }
        End::Discarded => {
            separate(text);
            text.push_str(&bart!(
                Cell,
                "{earlier} discarded by restart while still running"
            ));
            Status::Failed
        }
        End::TimedOut => {
            separate(text);
            let _ = if matches!(participation, Participation::Started) {
                write!(
                    text,
                    "{}",
                    bart!(
                        Cell,
                        "{earlier} timed out after {}s and is still running; the next call waits for it, or pass restart: true",
                        timeout.as_secs()
                    )
                )
            } else {
                write!(
                    text,
                    "{}",
                    bart!(
                        NotRun,
                        "{earlier} is still running after {}s more; call again to keep waiting, or pass restart: true",
                        timeout.as_secs()
                    )
                )
            };
            Status::Failed
        }
        lost => {
            let cause = match lost {
                End::Broken(error) => format!("interpreter failed: {error}"),
                _ => "interpreter exited".into(),
            };
            separate(text);
            let _ = write!(
                text,
                "{}",
                bart!(
                    Cell,
                    "{earlier} {cause}; it restarts on the next call without its state"
                )
            );
            Status::Failed
        }
    }
}

fn shown(text: &str, header: &str) -> bool {
    text.match_indices(header).any(|(start, _)| {
        let rest = &text[start + header.len()..];
        rest.starts_with([']', ';'])
            && rest
                .split('\n')
                .next()
                .is_some_and(|line| line.contains(']'))
    })
}

pub(super) struct ProcessSummaries {
    record: crate::tool::output::StatusWriter,
    lines: Vec<ProcessSummary>,
    bytes: usize,
    limit: usize,
    omitted: u64,
}

struct ProcessSummary {
    reference: crate::tool::output::Name,
    state: String,
    details: Option<String>,
}

impl ProcessSummary {
    fn line(&self) -> String {
        let reference = self.reference.reference();
        let state = &self.state;
        match &self.details {
            Some(details) => bart!(Run, "{reference} {state}; {details}"),
            None => bart!(Run, "{reference} {state}"),
        }
    }
}

impl ProcessSummaries {
    const OMISSION_RESERVE: usize = 80;

    fn new(collector: &Collector) -> Self {
        Self {
            record: collector.status_writer(),
            lines: Vec::new(),
            bytes: 0,
            limit: (collector.budget() / 2).saturating_sub(Self::OMISSION_RESERVE),
            omitted: 0,
        }
    }

    pub(super) fn push(
        &mut self,
        summary: serde_json::Value,
        reference: crate::tool::output::Name,
        result: &crate::tool::process::ProgramResult,
    ) {
        self.record.write(&summary);
        let details = result.details();
        if result.outcome != crate::history::JobOutcome::Exit(0)
            || result.timed_out
            || details.is_some()
        {
            let summary = ProcessSummary {
                reference,
                state: result.state(),
                details,
            };
            let bytes = summary.line().len() + 1;
            if self.omitted > 0 || self.lines.len() == 64 || self.bytes + bytes > self.limit {
                self.omitted += 1;
            } else {
                self.bytes += bytes;
                self.lines.push(summary);
            }
        }
    }

    fn finish(self, text: &str) -> (String, Vec<String>) {
        let mut lines = Vec::new();
        for summary in self.lines {
            let reference = summary.reference.reference();
            let header =
                crate::marker::open(bart!(@name Run), &format!("{reference} {}", summary.state));
            if summary.details.is_none() && shown(text, &header) {
                continue;
            }
            lines.push(summary.line());
        }
        let notice = self.record.finish(self.omitted);
        (notice, lines)
    }
}

pub(super) struct Effects {
    pub(super) process_summaries: ProcessSummaries,
    pub(super) processes: u32,
    pub(super) jobs: Vec<u32>,
    pub(super) images: Vec<InlineImage>,
    pub(super) changed: Vec<String>,
    pub(super) diffs: Vec<Diff>,
}

impl Effects {
    pub(super) fn new(collector: &Collector) -> Self {
        Self {
            process_summaries: ProcessSummaries::new(collector),
            processes: 0,
            jobs: Vec::new(),
            images: Vec::new(),
            changed: Vec::new(),
            diffs: Vec::new(),
        }
    }
}

pub(super) fn diff(path: String, before: &str, after: &str) -> Option<Diff> {
    let hunks = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .to_string();
    (!hunks.is_empty()).then_some(Diff { path, hunks })
}

pub(super) fn separate(text: &mut String) {
    text.truncate(text.trim_end_matches('\n').len());
    if !text.is_empty() {
        text.push('\n');
    }
}
