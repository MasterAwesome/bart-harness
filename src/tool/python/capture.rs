use std::{fmt::Write as _, time::Duration};

use similar::TextDiff;

use super::host::End;
use crate::{
    history::InlineImage,
    tool::{Diff, Output, Run, Status, output::Collector},
};

const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

pub(super) struct Capture {
    pub(super) collector: Collector,
    pub(super) effects: Effects,
    pub(super) stream: crate::tool::live::Stream,
    pub(super) cancelled: bool,
}

impl Capture {
    pub(super) fn new(collector: Collector, mut effects: Effects, live: &Run) -> Self {
        effects.process_summaries.record = Some(collector.status_writer());
        Self {
            collector,
            effects,
            stream: live.progress().stream(),
            cancelled: false,
        }
    }
    pub(super) async fn finish(
        mut self,
        end: End,
        timeout: Duration,
        ran: bool,
        note: Option<String>,
        prior: String,
    ) -> Output {
        tokio::task::spawn_blocking(move || {
            self.stream.finish(|line| self.collector.line(line));
            let (lifecycle, status) = result_notes(
                end,
                timeout,
                &self.effects.changed,
                note.as_deref(),
                &prior,
                ran,
            );
            let cell = self.collector.cell();
            let (mut text, delivery, saved) = self.collector.finish();
            // Trim trailing newlines only when all output fits. Otherwise the
            // displayed byte count must still match the text we kept.
            if delivery.is_empty() {
                text.truncate(text.trim_end_matches('\n').len());
            }
            let (mut notes, summaries) = self.effects.process_summaries.finish();
            if !lifecycle.is_empty() {
                separate(&mut notes);
                notes.push_str(&lifecycle);
            }
            for summary in &summaries {
                separate(&mut notes);
                notes.push_str(summary);
            }

            if !delivery.is_empty() {
                separate(&mut notes);
                notes.push_str(&delivery);
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
                cell,
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
    prior: &str,
    ran: bool,
) -> (String, Status) {
    let mut notes = String::new();
    let status = conclude(&mut notes, end, timeout, ran);
    if status == Status::Failed && !changed.is_empty() {
        let _ = write!(
            notes,
            "\n{}",
            bart!(Cell, "before failing it {}", changed.join("; "))
        );
    }
    if let Some(note) = note {
        notes.insert_str(0, &format!("{note}\n"));
    }
    if !prior.is_empty() {
        notes.push_str(&format!("\n{} {prior}", bart!(Cell, "earlier cell")));
    }
    (notes, status)
}

pub(super) fn denied_prior(mut prior: Output, reason: String) -> Output {
    let earlier = prior.cell.take();
    if let Some(cell) = earlier {
        prior.notes.insert_str(
            0,
            &format!("{}\n", bart!(Cell, "earlier output is cell[{cell}].output")),
        );
    }
    let prefix = match prior.prefix_len {
        Some(end) => prior.text[..end].to_owned(),
        None => std::mem::take(&mut prior.text),
    };
    prior.notes.insert_str(
        0,
        &format!("{reason}\n{}\n", bart!(Cell, "earlier cell's result")),
    );
    prior.prefix_len = Some(prefix.len());
    prior.text = crate::tool::output::render(&prefix, &prior.notes);
    prior.status = Status::Failed;
    prior
}

pub(super) fn conclude(text: &mut String, end: End, timeout: Duration, ran: bool) -> Status {
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
                        "timed out after {}s and was interrupted; Python state kept",
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
            text.push_str(bart!(const Cell, "cancelled; Python state kept, effects so far remain"));
            Status::Failed
        }
        End::Discarded => {
            separate(text);
            text.push_str(
                bart!(const Cell, "the restart discarded an earlier cell that was still running"),
            );
            Status::Failed
        }
        End::TimedOut => {
            separate(text);
            let _ = if ran {
                write!(
                    text,
                    "{}",
                    bart!(
                        Cell,
                        "timed out after {}s and is still running; the next call waits for it, or pass restart: true",
                        timeout.as_secs()
                    )
                )
            } else {
                write!(
                    text,
                    "{}",
                    bart!(
                        NotRun,
                        "the earlier cell is still running after {}s more; call again to keep waiting, or pass restart: true",
                        timeout.as_secs()
                    )
                )
            };
            Status::Failed
        }
        lost => {
            let cause = match lost {
                End::Broken(error) => format!("the interpreter failed: {error}"),
                _ => "the interpreter exited".into(),
            };
            separate(text);
            let _ = write!(
                text,
                "{}",
                bart!(
                    Cell,
                    "{cause}; it restarts on the next call without its state"
                )
            );
            Status::Failed
        }
    }
}

#[derive(Default)]
pub(super) struct ProcessSummaries {
    record: Option<crate::tool::output::StatusWriter>,
    lines: Vec<String>,
    bytes: usize,
    omitted: u64,
}

impl ProcessSummaries {
    const OMISSION_RESERVE: usize = 80;

    pub(super) fn push(&mut self, summary: serde_json::Value, line: String) {
        if let Some(record) = &mut self.record {
            record.write(&summary);
        }
        let plain = summary.as_object().is_some_and(|s| s.len() == 2);
        if !(plain && summary["return_code"] == 0) {
            self.append(line);
        }
    }

    fn append(&mut self, line: String) {
        let limit = crate::tool::OUTPUT_BUDGET_BYTES / 2 - Self::OMISSION_RESERVE;
        if self.omitted > 0 || self.lines.len() == 64 || self.bytes + line.len() + 1 > limit {
            self.omitted += 1;
        } else {
            self.bytes += line.len() + 1;
            self.lines.push(line);
        }
    }

    fn finish(mut self) -> (String, Vec<String>) {
        let notice = self
            .record
            .take()
            .map_or_else(String::new, |record| record.finish(self.omitted));
        if self.omitted > 0 {
            self.lines
                .push(bart!(Status, "{} more not shown", self.omitted));
        }
        (notice, self.lines)
    }
}

#[derive(Default)]
pub(super) struct Effects {
    pub(super) process_summaries: ProcessSummaries,
    pub(super) processes: u32,
    pub(super) jobs: Vec<u32>,
    pub(super) images: Vec<InlineImage>,
    pub(super) changed: Vec<String>,
    pub(super) diffs: Vec<Diff>,
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
    if !text.is_empty() {
        text.push('\n');
    }
}
