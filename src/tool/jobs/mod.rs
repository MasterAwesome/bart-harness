use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
    },
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::history::{JobOutcome, Notification, NotificationKind};

use super::{
    Context,
    {output::Collector, process::Process},
};

const MAX_OUTSTANDING: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cleanup {
    Settled,
    Unconfirmed,
}

impl Cleanup {
    pub(crate) fn notice(self) -> &'static str {
        match self {
            Self::Settled => "Tracked background jobs stopped.",
            Self::Unconfirmed => {
                "Cancellation was requested, but cleanup was not confirmed; descendants may remain."
            }
        }
    }
}

pub(crate) struct Jobs {
    next_id: AtomicU32,
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    running: HashMap<u32, Running>,
    notifications: Vec<Notification>,
}

pub(super) struct Request {
    command: super::execution::Argv,
    pattern: Option<Regex>,
    pub(super) outside: Option<String>,
}

impl Request {
    pub(super) fn parse(arguments: Value, escape: super::Escape) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            argv: Vec<String>,
            notify: Option<String>,
            #[serde(default)]
            env: std::collections::BTreeMap<String, String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EscapingWire {
            argv: Vec<String>,
            notify: Option<String>,
            #[serde(default)]
            env: std::collections::BTreeMap<String, String>,
            sandbox: bool,
            reason: Option<String>,
        }
        let invalid = |error: serde_json::Error| format!("invalid bg_run request: {error}");
        let (argv, notify, outside, env) = match escape {
            super::Escape::Withheld => {
                let wire: Wire = serde_json::from_value(arguments).map_err(invalid)?;
                (wire.argv, wire.notify, None, wire.env)
            }
            super::Escape::Offered => {
                let wire: EscapingWire = serde_json::from_value(arguments).map_err(invalid)?;
                let outside = super::execution::outside(wire.sandbox, wire.reason)?;
                (wire.argv, wire.notify, outside, wire.env)
            }
        };
        let command = super::execution::Argv::parse(argv)?.with_env(env)?;
        let pattern = notify.map(|pattern| {
            if pattern.len() > super::execution::COMMAND_BYTES {
                return Err("notify pattern must be at most 32 KiB".to_owned());
            }
            Regex::new(&pattern).map_err(|error| format!("invalid notify regex: {error}; use Rust regex syntax (no look-around or backreferences)"))
        }).transpose()?;
        Ok(Self {
            command,
            pattern,
            outside,
        })
    }

    pub(super) fn review_label(&self) -> String {
        self.command.review_label()
    }
}

struct Running {
    background: bool,
    command_exited: bool,
    cancel: CancellationToken,
    command: String,
    capture: Option<Value>,
}

const MATCHED_LINE_BYTES: usize = 200;

#[derive(Serialize)]
pub(super) struct Started {
    pub(super) id: u32,
    command: String,
}

fn finished(
    id: u32,
    command: &str,
    output: &str,
    capture: crate::history::SavedArtifact,
    outcome: JobOutcome,
) -> Notification {
    let first = command.lines().next().unwrap_or_default();
    let mut summary: String = first[..first.floor_char_boundary(200)].into();
    if summary.len() < command.len() {
        summary.push_str(" …");
    }
    Notification {
        text: format!("[background job {id} finished: {summary}]\n{output}").into(),
        kind: NotificationKind::JobFinished {
            id,
            outcome,
            command: command.to_owned(),
            capture,
        },
    }
}

impl Jobs {
    pub(crate) fn after(highest: u32) -> Self {
        Self {
            next_id: AtomicU32::new(highest.saturating_add(1)),
            state: Mutex::default(),
            changed: Notify::new(),
        }
    }

    pub(super) fn reserve(self: &Arc<Self>, command: &str) -> Result<Reservation, String> {
        let mut state = self.state();
        if state.running.len()
            + state
                .notifications
                .iter()
                .filter(|note| note.terminal_job().is_some())
                .count()
            >= MAX_OUTSTANDING
        {
            return Err(format!(
                "{MAX_OUTSTANDING} background jobs are running or waiting to report; finish this cell before starting more"
            ));
        }
        let id = self
            .next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| "background job IDs are exhausted; start a new session".to_owned())?;
        let cancel = CancellationToken::new();
        state.running.insert(
            id,
            Running {
                background: false,
                command_exited: false,
                cancel: cancel.clone(),
                command: command.to_owned(),
                capture: None,
            },
        );
        Ok(Reservation {
            jobs: Arc::clone(self),
            id,
            cancel,
        })
    }

    pub(super) fn adopt(
        self: &Arc<Self>,
        reservation: Reservation,
        process: Process,
        mut collector: Collector,
        live: &Arc<super::Live>,
        deadline: std::time::Instant,
    ) -> Result<u32, Box<(Reservation, Process, Collector)>> {
        let mut state = self.state();
        if reservation.cancel.is_cancelled() || std::time::Instant::now() >= deadline {
            return Err(Box::new((reservation, process, collector)));
        }
        let id = reservation.id;
        if collector.link(crate::tool::output::Name::Job(id)).is_err() {
            return Err(Box::new((reservation, process, collector)));
        }
        collector.line("\n[foreground shell exited; descendant output continues below]");
        state
            .running
            .get_mut(&id)
            .expect("reserved job slot")
            .command_exited = true;
        self.launch(state, reservation, process, collector, live, None);
        Ok(id)
    }

    fn launch(
        self: &Arc<Self>,
        mut state: MutexGuard<'_, State>,
        reservation: Reservation,
        process: Process,
        collector: Collector,
        live: &Arc<super::Live>,
        mut pattern: Option<Regex>,
    ) {
        let id = reservation.id;
        let command = state
            .running
            .get(&id)
            .expect("reserved job slot")
            .command
            .clone();
        let job = state.running.get_mut(&id).expect("reserved job slot");
        job.background = true;
        let [stdout_file, stderr_file] = collector.saved_streams([false; 2]);
        job.capture = Some(serde_json::json!({
            "return_code": null, "outcome": null, "timed_out": false,
            "cleanup_error": null, "retained_job": null,
            "output_file": collector.saved(), "stdout_file": stdout_file, "stderr_file": stderr_file,
        }));
        let mut run = live.start(&format!("job {id}: {command}"), &command, Some(id));
        run.progress().file(collector.path());
        let jobs = Arc::downgrade(self);
        let cancel = reservation.cancel.clone();
        let artifacts_storage = Arc::clone(collector.storage());
        tokio::spawn(async move {
            let (mut output, result) = process
                .finish_watched(
                    collector,
                    &mut run,
                    |line| {
                        if let Some(jobs) = jobs.upgrade() {
                            jobs.matched(id, &mut pattern, line);
                        }
                    },
                    |outcome| {
                        if let Some(jobs) = jobs.upgrade() {
                            let mut state = jobs.state();
                            state.running.get_mut(&id).expect("running job").command_exited = true;
                            state.notifications.push(Notification {
                                text: format!("[background job {id} command exited; descendants retained as job {id}: {outcome:?}]\nThe requested command exited, but its descendants remain owned as job {id}. Use its result to continue; stop temporary descendants with job[{id}].kill() when no longer needed, without stopping intentional services.").into(),
                                kind: NotificationKind::JobCommandExited { id, outcome, command: command.clone() },
                            });
                            jobs.changed.notify_waiters();
                        }
                    },
                    cancel,
                )
                .await;
            let outcome = result.outcome;
            let capture = result.output_file.clone();
            let saved = tokio::task::spawn_blocking(move || {
                super::output::save(
                    &artifacts_storage,
                    super::output::Name::JobResult(id),
                    &serde_json::to_vec(&result).expect("serializable result"),
                )
            })
            .await
            .expect("job result worker");
            if let Err(error) = saved {
                output.push_str(&format!("\n[Bart: job[{id}] result unavailable: {error}]"));
            }
            let mut state = reservation.jobs.state();
            if let Some(job) = state.running.remove(&id) {
                state
                    .notifications
                    .push(finished(id, &job.command, &output, capture, outcome));
            }
            drop(state);
            reservation.jobs.changed.notify_waiters();
        });
    }

    fn matched(&self, id: u32, pattern: &mut Option<Regex>, line: &str) {
        let Some(regex) = pattern.take_if(|regex| regex.is_match(line)) else {
            return;
        };
        let shown = line.floor_char_boundary(MATCHED_LINE_BYTES);
        let rest = if shown < line.len() {
            format!(
                " [… {} more chars]",
                crate::tool::thousands(line[shown..].chars().count())
            )
        } else {
            String::new()
        };
        self.state().notifications.push(Notification {
            text: format!(
                "[background job {id} matched {:?}]\n{}{rest}",
                regex.as_str(),
                &line[..shown]
            )
            .into(),
            kind: NotificationKind::JobMatched { id },
        });
        self.changed.notify_waiters();
    }

    pub(super) async fn start_command(
        self: &Arc<Self>,
        request: Request,
        context: &Context,
        launch: super::process::Launch,
    ) -> Result<Started, String> {
        let Request {
            command, pattern, ..
        } = request;
        let reservation = self.reserve(&command.label())?;
        let id = reservation.id;
        let mut collector = Collector::open(
            context.output_budget,
            &context.artifacts,
            crate::tool::output::Name::Job(id),
            &[],
        )
        .await
        .map_err(|error| format!("cannot create the job's output file: {error}"))?;
        collector.start_streams().await;
        let process = match Process::spawn_background(&command, context, launch).await {
            Ok(process) => process,
            Err(error) => {
                collector.discard();
                return Err(error);
            }
        };
        self.launch(
            self.state(),
            reservation,
            process,
            collector,
            &context.live,
            pattern,
        );
        Ok(Started {
            id,
            command: command.label(),
        })
    }

    pub(super) async fn snapshot(&self, id: u32, context: &Context) -> Result<Value, String> {
        if let Some(job) = self.state().running.get(&id) {
            return job
                .capture
                .clone()
                .ok_or_else(|| format!("job[{id}] is not ready"));
        }
        let path = context.spill_dir.join(format!("job-{id}.json"));
        tokio::task::spawn_blocking(move || {
            let bytes = crate::persist::read(&path, 64 * 1024)
                .map_err(|_| format!("cannot read job[{id}] result"))?
                .ok_or_else(|| format!("job[{id}] has no retained result; it may be unknown or lost during session shutdown"))?;
            let result: super::process::ProgramResult = serde_json::from_slice(&bytes)
                .map_err(|_| format!("job[{id}] result is incomplete or invalid"))?;
            Ok(serde_json::to_value(result).expect("serializable result"))
        }).await.map_err(|error| format!("job result worker failed: {error}"))?
    }

    pub(super) async fn kill(&self, id: u32) -> Result<Option<String>, String> {
        let requested = {
            let state = self.state();
            let Some(job) = state.running.get(&id) else {
                return if id > 0 && id < self.next_id.load(Ordering::Relaxed) {
                    Ok(None)
                } else {
                    Err(format!("no job {id}"))
                };
            };
            let requested = !job.cancel.is_cancelled();
            job.cancel.cancel();
            requested
        };
        tokio::time::timeout(
            crate::supervise::CLEANUP + std::time::Duration::from_secs(2),
            self.wait_for(Some(id)),
        )
        .await
        .map_err(|_| format!("job {id} cleanup deadline exceeded; descendants may remain"))?;
        Ok(requested.then(|| format!("cancellation requested for job {id}; effects unknown")))
    }

    pub(crate) fn running_count(&self) -> usize {
        let state = self.state();
        state
            .running
            .values()
            .filter(|job| job.background && !job.cancel.is_cancelled())
            .count()
    }

    #[must_use]
    pub(crate) fn any(&self) -> bool {
        let state = self.state();
        !state.running.is_empty() || !state.notifications.is_empty()
    }

    pub(crate) fn pending_commands(&self) -> bool {
        let state = self.state();
        !state.notifications.is_empty() || state.running.values().any(|job| !job.command_exited)
    }

    pub(crate) async fn wake_ready(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.state().notifications.is_empty() {
                return;
            }
            changed.await;
        }
    }

    pub(crate) fn take_notifications(&self) -> Vec<Notification> {
        std::mem::take(&mut self.state().notifications)
    }

    pub(crate) fn stop_all(&self) {
        let state = self.state();
        for job in state.running.values() {
            job.cancel.cancel();
        }
    }

    pub(crate) async fn settled(&self) -> Cleanup {
        if tokio::time::timeout(
            crate::supervise::CLEANUP + std::time::Duration::from_secs(2),
            self.wait_for(None),
        )
        .await
        .is_err()
        {
            crate::terminal_text::stderr_line(format_args!(
                "bart: job cleanup deadline exceeded; descendants may remain"
            ));
            Cleanup::Unconfirmed
        } else {
            Cleanup::Settled
        }
    }

    async fn wait_for(&self, id: Option<u32>) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self
                .state()
                .running
                .keys()
                .any(|key| id.is_none_or(|id| *key == id))
            {
                return;
            }
            changed.await;
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect(crate::NOT_POISONED)
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.stop_all();
    }
}

pub(super) struct Reservation {
    jobs: Arc<Jobs>,
    pub(super) id: u32,
    pub(super) cancel: CancellationToken,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.jobs.state().running.remove(&self.id);
        self.jobs.changed.notify_waiters();
    }
}

pub(super) struct CommandCancellation {
    jobs: Arc<Jobs>,
    id: u32,
}
impl CommandCancellation {
    pub(super) fn cancel(self) {
        let state = self.jobs.state();
        if let Some(command) = state.running.get(&self.id) {
            command.cancel.cancel();
        }
    }
}
impl Reservation {
    pub(super) fn cancellation(&self) -> CommandCancellation {
        CommandCancellation {
            jobs: Arc::clone(&self.jobs),
            id: self.id,
        }
    }
}

#[cfg(test)]
mod tests;
