use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use crate::terminal_text::TerminalText;

use super::Status;

const MAX_ENTRIES: usize = 64;
use crate::detail_excerpt::{self, CODE_BYTES as MAX_CODE_BYTES, OUTPUT_BYTES as MAX_OUTPUT_BYTES};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Running,
    Finished,
    Failed,
    Interrupted,
}

impl State {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone)]
pub(crate) struct Summary {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) job: Option<u32>,
    pub(crate) order: u64,
    pub(crate) state: State,
}

pub(crate) struct Snapshot {
    pub(crate) summary: Summary,
    pub(crate) code: String,
    pub(crate) code_complete: bool,
    pub(crate) output: String,
    pub(crate) output_complete: bool,
    pub(crate) saved: Option<PathBuf>,
}

struct Entry {
    summary: Summary,
    code: String,
    code_complete: bool,
    output: VecDeque<u8>,
    output_complete: bool,
    partial: String,
    saved: Option<PathBuf>,
    activities: Vec<Activity>,
    omitted_activities: usize,
}

// Builtin calls from a cell, saved for tools-only display.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Activities {
    pub(crate) rows: Vec<Activity>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) omitted: usize,
}

impl Activities {
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.omitted == 0
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Activity {
    pub(crate) title: String,
    pub(crate) state: State,
}

#[derive(Default)]
struct Entries {
    next: u64,
    entries: BTreeMap<u64, Entry>,
}

#[derive(Default)]
pub(crate) struct Live(Mutex<Entries>, tokio::sync::Notify);

impl Live {
    pub(crate) async fn changed(&self) {
        self.1.notified().await;
    }

    fn state(&self) -> MutexGuard<'_, Entries> {
        self.0.lock().expect(crate::NOT_POISONED)
    }

    pub(crate) fn start(self: &Arc<Self>, title: &str, code: &str, job: Option<u32>) -> Run {
        let mut state = self.state();
        while state.entries.len() >= MAX_ENTRIES {
            let oldest = state
                .entries
                .iter()
                .find(|(_, entry)| entry.summary.state != State::Running)
                .map(|(&id, _)| id);
            let Some(id) = oldest.or_else(|| state.entries.keys().next().copied()) else {
                break;
            };
            state.entries.remove(&id);
        }
        state.next = state.next.saturating_add(1);
        let id = state.next;
        let excerpt = detail_excerpt::head(code, MAX_CODE_BYTES);
        let code_complete = excerpt.len() == code.len();
        let code = excerpt.to_owned();
        let title = title
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(80)
            .collect();
        state.entries.insert(
            id,
            Entry {
                summary: Summary {
                    id,
                    title,
                    job,
                    order: id,
                    state: State::Running,
                },
                code,
                code_complete,
                output: VecDeque::new(),
                output_complete: true,
                partial: String::new(),
                saved: None,
                activities: Vec::new(),
                omitted_activities: 0,
            },
        );
        self.1.notify_one();
        Run {
            progress: Progress {
                live: Arc::clone(self),
                id,
            },
            terminal: TerminalText::default(),
            finished: false,
        }
    }

    pub(crate) fn activities(&self, id: u64) -> Option<Activities> {
        self.state().entries.get(&id).map(|entry| Activities {
            rows: entry.activities.clone(),
            omitted: entry.omitted_activities,
        })
    }

    pub(crate) fn jobs(&self) -> Vec<Summary> {
        self.state()
            .entries
            .values()
            .filter(|entry| entry.summary.job.is_some())
            .map(|entry| entry.summary.clone())
            .collect()
    }

    pub(crate) fn snapshot(&self, id: u64) -> Option<Snapshot> {
        let state = self.state();
        let entry = state.entries.get(&id)?;
        let room = MAX_OUTPUT_BYTES.saturating_sub(entry.partial.len());
        let bytes: Vec<_> = entry.output.iter().copied().collect();
        let output = String::from_utf8_lossy(&bytes);
        Some(Snapshot {
            summary: entry.summary.clone(),
            code: entry.code.clone(),
            code_complete: entry.code_complete,
            output: format!("{}{}", detail_excerpt::tail(&output, room), entry.partial),
            output_complete: entry.output_complete && output.len() <= room,
            saved: entry.saved.clone(),
        })
    }
}

#[derive(Clone)]
pub(super) struct Progress {
    live: Arc<Live>,
    id: u64,
}

impl Progress {
    pub(super) fn activity(&self, title: &str) -> u64 {
        let mut entries = self.live.state();
        let Some(entry) = entries.entries.get_mut(&self.id) else {
            return 0;
        };
        if entry.activities.len() >= 1024 {
            entry.omitted_activities += 1;
            self.live.1.notify_one();
            return 0;
        }
        let title: String = title.chars().take(512).collect();
        entry.activities.push(Activity {
            title: crate::terminal_text::safe_text(
                &title,
                crate::terminal_text::TextMode::Diagnostic,
            ),
            state: State::Running,
        });
        self.live.1.notify_one();
        entry.activities.len() as u64
    }

    pub(super) fn finish_activity(&self, id: u64, state: State) {
        if let Some(entry) = self.live.state().entries.get_mut(&self.id)
            && let Some(activity) = id
                .checked_sub(1)
                .and_then(|id| entry.activities.get_mut(id as usize))
            && activity.state == State::Running
        {
            activity.state = state;
            self.live.1.notify_one();
        }
    }

    pub(super) fn scoped_activity(&self, title: &str) -> ActivityRun {
        ActivityRun {
            progress: self.clone(),
            id: self.activity(title),
        }
    }

    pub(super) fn file(&self, path: &Path) {
        if let Some(entry) = self.live.state().entries.get_mut(&self.id) {
            entry.saved = Some(path.to_owned());
            self.live.1.notify_one();
        }
    }

    pub(super) fn stream(&self) -> Stream {
        Stream {
            terminal: TerminalText::default(),
            live: self.clone(),
        }
    }

    pub(super) fn push(&self, bytes: &[u8]) {
        let mut terminal = TerminalText::default();
        self.render(&mut terminal, bytes, |_| {});
        self.render_end(terminal, |_| {});
    }

    fn render(&self, terminal: &mut TerminalText, bytes: &[u8], mut line: impl FnMut(&str)) {
        terminal.push(bytes, |text| {
            line(text);
            self.line(text);
        });
        if let Some(entry) = self.live.state().entries.get_mut(&self.id) {
            let partial = terminal.current_line();
            if entry.partial != partial {
                entry.partial = partial;
                self.live.1.notify_one();
            }
        }
    }

    fn render_end(&self, terminal: TerminalText, mut line: impl FnMut(&str)) {
        terminal.finish(|text| {
            line(text);
            self.line(text);
        });
    }

    fn line(&self, line: &str) {
        if let Some(entry) = self.live.state().entries.get_mut(&self.id) {
            append(entry, line);
            entry.partial.clear();
            self.live.1.notify_one();
        }
    }
}

pub(super) struct ActivityRun {
    progress: Progress,
    id: u64,
}

impl ActivityRun {
    pub(super) fn finish(&self, succeeded: bool) {
        self.progress.finish_activity(
            self.id,
            if succeeded {
                State::Finished
            } else {
                State::Failed
            },
        );
    }
}

impl Drop for ActivityRun {
    fn drop(&mut self) {
        self.progress.finish_activity(self.id, State::Interrupted);
    }
}

pub(crate) struct Run {
    progress: Progress,
    terminal: TerminalText,
    finished: bool,
}

impl Run {
    pub(crate) fn id(&self) -> u64 {
        self.progress.id
    }

    pub(crate) fn activities(&self) -> Activities {
        self.progress
            .live
            .activities(self.progress.id)
            .unwrap_or_default()
    }

    pub(super) fn progress(&self) -> &Progress {
        &self.progress
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        let mut terminal = std::mem::take(&mut self.terminal);
        self.progress.render(&mut terminal, bytes, |_| {});
        self.terminal = terminal;
    }

    pub(crate) fn result(&mut self, text: &str) {
        self.push(b"\n[result]\n");
        self.push(text.as_bytes());
    }

    pub(crate) fn finish(&mut self, status: Status) {
        self.end(if status == Status::Succeeded {
            State::Finished
        } else {
            State::Failed
        });
    }

    fn end(&mut self, state: State) {
        let mut entries = self.progress.live.state();
        entries.next = entries.next.saturating_add(1);
        let order = entries.next;
        if let Some(entry) = entries.entries.get_mut(&self.progress.id) {
            std::mem::take(&mut self.terminal).finish(|line| append(entry, line));
            entry.partial.clear();
            for activity in &mut entry.activities {
                if activity.state == State::Running {
                    activity.state = State::Interrupted;
                }
            }
            entry.summary.state = state;
            entry.summary.order = order;
        }
        self.finished = true;
        self.progress.live.1.notify_one();
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if !self.finished {
            self.end(State::Interrupted);
        }
    }
}

pub(super) struct Stream {
    terminal: TerminalText,
    live: Progress,
}

impl Stream {
    pub(super) fn pending_bytes(&self) -> usize {
        self.terminal.current_line_bytes()
    }

    pub(super) fn redirect(&mut self, live: &Progress) {
        self.live = live.clone();
    }

    pub(super) fn progress(&self) -> Progress {
        self.live.clone()
    }

    pub(super) fn render(&mut self, bytes: &[u8], line: impl FnMut(&str)) {
        self.live.render(&mut self.terminal, bytes, line);
    }

    pub(super) fn finish(&mut self, line: impl FnMut(&str)) {
        self.live
            .render_end(std::mem::take(&mut self.terminal), line);
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.finish(|_| {});
    }
}

fn append(entry: &mut Entry, line: &str) {
    let output = &mut entry.output;
    output.extend(line.as_bytes());
    output.push_back(b'\n');
    let omitted = output.len().saturating_sub(MAX_OUTPUT_BYTES);
    entry.output_complete &= omitted == 0;
    output.drain(..omitted);
    while output.front().is_some_and(|&byte| byte & 0xc0 == 0x80) {
        output.pop_front();
    }
}
