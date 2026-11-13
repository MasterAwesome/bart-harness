use std::{
    fmt::Write as _,
    io,
    os::fd::OwnedFd,
    os::unix::process::ExitStatusExt as _,
    process::ExitStatus,
    time::{Duration, Instant},
};

use tokio::{io::AsyncReadExt, net::unix::pipe, time};

use crate::{
    child::Prepared as Command,
    history::{JobOutcome, SavedArtifact},
};

use super::output::{CaptureFailure, Collector, SavedStream};

use super::{Confinement, Context, Run, live::Progress};

#[cfg(target_os = "linux")]
mod linux;

#[derive(Default)]
struct Observation {
    timed_out: bool,
    cleanup_error: Option<String>,
}

const DRAIN: Duration = Duration::from_millis(200);

pub(super) struct Process {
    child: crate::child::Owner,
    pipe: pipe::Receiver,
    stderr: pipe::Receiver,
    observation: std::sync::Arc<std::sync::Mutex<Observation>>,
    started: Instant,
    reservation: Option<super::jobs::Reservation>,
    jobs: std::sync::Arc<super::Jobs>,
    live: std::sync::Arc<super::Live>,
    stream: Option<super::live::Stream>,
    exited: Option<ExitStatus>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(in crate::tool) struct ProgramResult {
    #[serde(rename = "return_code")]
    pub(in crate::tool) code: i32,
    pub(in crate::tool) output_file: SavedArtifact,
    pub(in crate::tool) stdout_file: SavedStream,
    pub(in crate::tool) stderr_file: SavedStream,
    pub(in crate::tool) outcome: JobOutcome,
    pub(in crate::tool) timed_out: bool,
    pub(in crate::tool) cleanup_error: Option<String>,
    #[serde(rename = "retained_job")]
    pub(in crate::tool) adopted: Option<u32>,
}

pub(super) const RUNNING: &str = "running";

pub(super) enum Launch {
    Session,
    Outside(crate::approval::Outside),
}

impl Process {
    pub(super) async fn spawn(
        command: &super::execution::Argv,
        context: &Context,
        within: Launch,
    ) -> Result<Self, String> {
        let reservation = context.jobs.reserve(command)?;
        let mut process = Self::spawn_command(command, context, within).await?;
        process.reservation = Some(reservation);
        Ok(process)
    }

    pub(super) async fn spawn_background(
        command: &super::execution::Argv,
        context: &Context,
        within: Launch,
    ) -> Result<Self, String> {
        Self::spawn_command(command, context, within).await
    }

    async fn spawn_command(
        command: &super::execution::Argv,
        context: &Context,
        within: Launch,
    ) -> Result<Self, String> {
        let confinement = match within {
            Launch::Session => context.confinement(),
            Launch::Outside(_) => Confinement::Unrestricted,
        };
        let started = Instant::now();
        let failed = |error: io::Error| {
            format!(
                "cannot start program {:?}: {}",
                command.args()[0],
                crate::terminal_text::io_reason(&error)
            )
        };
        let (reader, writer) = io::pipe().map_err(failed)?;
        let (err_reader, err_writer) = io::pipe().map_err(failed)?;
        let mut program = launch_in(
            &command.args()[0],
            &Place {
                confinement,
                ..context.place()
            },
            command.env(),
            &command.cwd(&context.cwd),
        )?;
        program.retain_descendants();
        program.exit_on_exec();
        program
            .args(&command.args()[1..])
            .stdin(std::process::Stdio::null())
            .stdout(writer)
            .stderr(err_writer);
        let child = crate::child::Owner::spawn(&mut program)
            .await
            .map_err(failed)?;
        let pipe = pipe::Receiver::from_owned_fd(OwnedFd::from(reader)).map_err(failed)?;
        Ok(Self {
            child,
            pipe,
            stderr: pipe::Receiver::from_owned_fd(OwnedFd::from(err_reader)).map_err(failed)?,
            observation: std::sync::Arc::default(),
            started,
            reservation: None,
            jobs: std::sync::Arc::clone(&context.jobs),
            live: std::sync::Arc::clone(&context.live),
            stream: None,
            exited: None,
        })
    }

    pub(super) fn cancellation(&self) -> super::jobs::CommandCancellation {
        self.reservation
            .as_ref()
            .expect("foreground command reserved")
            .cancellation()
    }

    pub(super) async fn finish_watched(
        self,
        handoff: tokio::sync::oneshot::Receiver<super::jobs::Handoff>,
        collector: Collector,
        live: &mut Run,
        observe: impl FnMut(&str),
        on_exit: impl FnMut(JobOutcome),
        cancel: tokio_util::sync::CancellationToken,
    ) -> Option<(String, ProgramResult, Completion)> {
        let progress = live.progress().clone();
        let observation = std::sync::Arc::clone(&self.observation);
        let (mut text, saved, outcome, completion, streams) = match self
            .collect(
                collector,
                &progress,
                Collection::Background { handoff, cancel },
                observe,
                on_exit,
            )
            .await
        {
            Collected::Done(text, saved, outcome, completion, streams) => {
                (text, saved, outcome, completion, streams)
            }
            Collected::Detached => return None,
            Collected::Adopt(_) => unreachable!("background collections never adopt"),
        };
        text.truncate(text.trim_end_matches('\n').len());
        let result = ProgramResult::new(
            outcome,
            saved,
            *streams.expect("background streams started"),
            &observation,
            None,
        );
        live.result(&bart!(Job, "{}", completion.state(&result)));
        if !completion.cancelled {
            live.finish(if outcome == JobOutcome::Exit(0) {
                super::Status::Succeeded
            } else {
                super::Status::Failed
            });
        }
        Some((text, result, completion))
    }

    pub(super) async fn finish_result(
        self,
        mut collector: Collector,
        live: &Progress,
        timeout: Duration,
    ) -> ProgramResult {
        let deadline = self.started + timeout;
        collector.start_streams().await;
        let observation = std::sync::Arc::clone(&self.observation);
        let mut result = self
            .collect(
                collector,
                live,
                Collection::Foreground(timeout),
                |_| {},
                |_| {},
            )
            .await;
        let mut adopted = None;
        let (outcome, output_file, [stdout_file, stderr_file]) = loop {
            match result {
                Collected::Detached => unreachable!("foreground commands cannot detach"),
                Collected::Done(_, saved, outcome, _, streams) => {
                    break (
                        outcome,
                        saved,
                        *streams.expect("foreground streams started"),
                    );
                }
                Collected::Adopt(adoption) => {
                    let (mut process, mut collector, outcome) = *adoption;
                    let reservation = process
                        .reservation
                        .take()
                        .expect("foreground slot reserved");
                    let jobs = std::sync::Arc::clone(&process.jobs);
                    let job_live = std::sync::Arc::clone(&process.live);
                    if !collector.persist_until(time::Instant::now() + DRAIN).await {
                        collector.capture_failed(CaptureFailure::DrainDeadline);
                    }
                    let streams = collector.saved_streams([false; 2]);
                    let saved = collector.saved();
                    match jobs.adopt(reservation, process, collector, &job_live, deadline) {
                        Ok(id) => {
                            adopted = Some(id);
                            break (outcome, saved, streams);
                        }
                        Err(rejected) => {
                            let (reservation, mut process, collector) = *rejected;
                            process.reservation = Some(reservation);
                            result = process
                                .collect(
                                    collector,
                                    live,
                                    Collection::Foreground(timeout),
                                    |_| {},
                                    |_| {},
                                )
                                .await;
                        }
                    }
                }
            }
        };
        ProgramResult::new(
            outcome,
            output_file,
            [stdout_file, stderr_file],
            &observation,
            adopted,
        )
    }

    async fn collect(
        mut self,
        mut collector: Collector,
        live: &Progress,
        mode: Collection,
        mut observe: impl FnMut(&str),
        mut on_exit: impl FnMut(JobOutcome),
    ) -> Collected {
        let (timeout, mut handoff, cancel) = match mode {
            Collection::Foreground(timeout) => (
                Some(timeout),
                None,
                tokio_util::sync::CancellationToken::new(),
            ),
            Collection::Background { handoff, cancel } => (None, Some(handoff), cancel),
        };
        let cancel = self
            .reservation
            .as_ref()
            .map_or(cancel, |reservation| reservation.cancel.clone());
        let mut stream = self.stream.take().unwrap_or_else(|| live.stream());
        stream.redirect(live);
        let mut buffer = vec![0; 64 * 1024];
        let mut err_buffer = vec![0; 64 * 1024];
        let mut status: Option<io::Result<ExitStatus>> = self.exited.map(Ok);
        let mut end_stdout = false;
        let mut end_stderr = false;
        let mut scope: Option<Result<crate::child::Empty, String>> = None;
        let mut adopt = false;
        let drain = time::sleep(Duration::MAX);
        tokio::pin!(drain);
        let deadline = time::sleep(timeout.map_or(Duration::MAX, |timeout| {
            timeout.saturating_sub(self.started.elapsed())
        }));
        tokio::pin!(deadline);
        let mut timed_out = false;
        let mut cancelled = false;
        let mut lines = Vec::new();
        while !(end_stdout && end_stderr && scope.is_some()) {
            tokio::select! {
                request = async {
                    match &mut handoff {
                        Some(receive) => receive.await.ok(),
                        None => std::future::pending().await,
                    }
                }, if !cancelled && !timed_out => {
                    handoff = None;
                    if let Some(reply) = request {
                        let result = async {
                            use std::os::fd::AsFd;
                            if !collector.persist_until(time::Instant::now() + DRAIN).await {
                                return Err(io::Error::other("cannot flush job output"));
                            }
                            self.child.detach(crate::supervise::handoff::Streams {
                                stdout: self.pipe.as_fd(),
                                stderr: self.stderr.as_fd(),
                            }).await
                        }.await.map_err(|error| error.to_string());
                        match result {
                            Ok(crate::child::DetachOutcome::Detached) => {
                                let _ = reply.send(super::jobs::HandoffResult::Detached);
                                return Collected::Detached;
                            }
                            Ok(crate::child::DetachOutcome::Finished) => {
                                let _ = reply.send(super::jobs::HandoffResult::Finished);
                                scope = Some(Ok(crate::child::Empty));
                                drain.as_mut().reset(time::Instant::now() + DRAIN);
                            }
                            Err(error) => {
                                let _ = reply.send(super::jobs::HandoffResult::Failed(error));
                                self.child.kill();
                                cancelled = true;
                                drain.as_mut().reset(time::Instant::now() + DRAIN);
                            }
                        }
                    }
                },
                (source, read) = async {
                    tokio::select! {
                        read = self.pipe.read(&mut buffer), if !end_stdout => (0, read),
                        read = self.stderr.read(&mut err_buffer), if !end_stderr => (1, read),
                    }
                }, if !(end_stdout && end_stderr) => match read {
                    Ok(0) => if source == 0 { end_stdout = true } else { end_stderr = true },
                    Err(error) => {
                        if source == 0 { end_stdout = true; } else { end_stderr = true; }
                        collector.capture_failed(CaptureFailure::Read(error));
                    },
                    Ok(n) => {
                        let buffer = if source == 0 { &buffer } else { &err_buffer };
                        collector.stream_bytes(source, &buffer[..n]);
                        stream.render(&buffer[..n], |line| {
                            if timeout.is_none() { lines.push(line.to_owned()); }
                            collector.line(line);
                        });
                        let flush_deadline = if timed_out || cancelled {
                            drain.deadline()
                        } else {
                            deadline.deadline().min(drain.deadline())
                        };
                        if !flush_with_cancel(&mut collector, flush_deadline, &cancel, &mut cancelled,
                            &mut self.child, drain.as_mut(), timeout.is_none()).await {
                            if timed_out || cancelled || status.is_some() {
                                collector.capture_failed(CaptureFailure::DrainDeadline);
                                break;
                            }
                            timed_out = true;
                            self.child.kill();
                            drain.as_mut().reset(time::Instant::now() + DRAIN);
                        }
                        for line in lines.drain(..) {
                            observe(&line);
                        }
                    }
                },
                event = async {
                    if status.is_none() { self.child.target_exit().await.map(Some) }
                    else { self.child.empty().await.map(|_| None) }
                }, if scope.is_none() => {
                    match event {
                        Ok(Some(exit)) => {
                            status = Some(Ok(exit.status));
                            if timeout.is_none() && exit.survivors && !cancelled && !cancel.is_cancelled() {
                                on_exit(exit_outcome(&exit.status));
                            }
                            if timeout.is_some() && time::Instant::now() >= deadline.deadline() {
                                timed_out = true;
                                self.child.kill();
                                drain.as_mut().reset(time::Instant::now() + DRAIN);
                            } else if exit.survivors && timeout.is_some() && !timed_out && !cancelled && !cancel.is_cancelled() {
                                self.exited = Some(exit.status);
                                adopt = true;
                                break;
                            }
                        }
                        Ok(None) => { scope = Some(Ok(crate::child::Empty)); drain.as_mut().reset(time::Instant::now() + DRAIN); }
                        Err(error) => { scope = Some(Err(error.to_string())); self.child.kill(); drain.as_mut().reset(time::Instant::now() + DRAIN); }
                    }
                }
                () = &mut drain => {
                    if !(end_stdout && end_stderr) {
                        collector.capture_failed(CaptureFailure::DrainDeadline);
                    }
                    break;
                },
                () = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    self.child.kill();
                    drain.as_mut().reset(time::Instant::now() + DRAIN);
                }
                () = &mut deadline, if !timed_out && scope.is_none() => {
                    timed_out = true;
                    self.child.kill();
                    drain.as_mut().reset(time::Instant::now() + DRAIN);
                },
            }
        }
        if let Some(mut receive) = handoff.take()
            && let Ok(reply) = receive.try_recv()
        {
            let _ = reply.send(super::jobs::HandoffResult::Finished);
        }
        if adopt {
            for source in 0..2 {
                for _ in 0..4 {
                    let pipe = if source == 0 {
                        &self.pipe
                    } else {
                        &self.stderr
                    };
                    match rustix::io::read(pipe, &mut buffer).map_err(io::Error::from) {
                        Ok(0) => break,
                        Ok(n) => {
                            collector.stream_bytes(source, &buffer[..n]);
                            stream.render(&buffer[..n], |line| {
                                if timeout.is_none() {
                                    lines.push(line.to_owned());
                                }
                                collector.line(line);
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => {
                            collector.capture_failed(CaptureFailure::Read(error));
                            break;
                        }
                    }
                }
            }
        } else {
            stream.finish(|line| {
                if timeout.is_none() {
                    lines.push(line.to_owned());
                }
                collector.line(line);
            });
        }
        if adopt {
            let outcome = exit_outcome(self.exited.as_ref().expect("adopted command exited"));
            self.stream = Some(stream);
            return Collected::Adopt(Box::new((self, collector, outcome)));
        }
        if scope.is_none() {
            self.child.kill();
            scope = Some(
                match time::timeout(
                    crate::supervise::CLEANUP + Duration::from_secs(1),
                    self.child.empty(),
                )
                .await
                {
                    Ok(Ok(empty)) => Ok(empty),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("process monitor cleanup deadline exceeded".to_owned()),
                },
            );
        }
        if matches!(scope, Some(Ok(_))) && status.is_none() {
            status = Some(self.child.target_exit().await.map(|exit| exit.status));
        }
        let (mut outcome, completion) =
            termination(status, self.started.elapsed().as_secs_f64(), cancelled);
        if timed_out {
            outcome = JobOutcome::Killed;
        }
        if let Some(Err(error)) = &scope {
            outcome = JobOutcome::Unknown;
            self.observation
                .lock()
                .expect(crate::NOT_POISONED)
                .cleanup_error = Some(error.clone());
        }
        self.observation
            .lock()
            .expect(crate::NOT_POISONED)
            .timed_out = timed_out;
        let (text, saved, streams) = tokio::task::spawn_blocking(move || {
            let streams = collector.finish_streams([end_stdout, end_stderr]);
            let (text, notes, saved) = collector.finish();
            let text = super::output::render(&text, &notes);
            (text, saved, streams)
        })
        .await
        .expect("output worker");
        for line in lines {
            observe(&line);
        }
        Collected::Done(text, saved, outcome, completion, streams.map(Box::new))
    }
}

impl ProgramResult {
    pub(super) fn state(&self) -> String {
        let mut state = self.outcome.label();
        if self.timed_out {
            state.push_str("; timed out");
        }
        state
    }

    pub(super) fn details(&self) -> Option<String> {
        let mut parts: Vec<_> = [
            ("cleanup failed", &self.cleanup_error),
            ("stdout incomplete", &self.stdout_file.incomplete),
            ("stderr incomplete", &self.stderr_file.incomplete),
        ]
        .into_iter()
        .filter_map(|(label, reason)| reason.as_ref().map(|reason| format!("{label}: {reason}")))
        .collect();
        if let Some(job) = self.adopted {
            parts.push(format!("left processes running as job[{job}]"));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    pub(super) fn payload(self, name: super::output::Name) -> serde_json::Value {
        let state = self.state();
        let mut value = serde_json::json!(self);
        value["state"] = state.into();
        value["reference"] = name.reference().into();
        value
    }

    fn new(
        outcome: JobOutcome,
        output_file: SavedArtifact,
        [stdout_file, stderr_file]: [SavedStream; 2],
        observation: &std::sync::Mutex<Observation>,
        adopted: Option<u32>,
    ) -> Self {
        let code = match outcome {
            JobOutcome::Exit(code) => code,
            JobOutcome::Signal(signal) => -signal,
            JobOutcome::Killed | JobOutcome::Unknown => -1,
        };
        let captured = observation.lock().expect(crate::NOT_POISONED);
        Self {
            code,
            output_file,
            stdout_file,
            stderr_file,
            outcome,
            timed_out: captured.timed_out,
            cleanup_error: captured.cleanup_error.clone(),
            adopted,
        }
    }
}

enum Collection {
    Foreground(Duration),
    Background {
        handoff: tokio::sync::oneshot::Receiver<super::jobs::Handoff>,
        cancel: tokio_util::sync::CancellationToken,
    },
}

enum Collected {
    Detached,
    Done(
        String,
        SavedArtifact,
        JobOutcome,
        Completion,
        Option<Box<[SavedStream; 2]>>,
    ),
    Adopt(Box<(Process, Collector, JobOutcome)>),
}

async fn flush_with_cancel(
    collector: &mut Collector,
    deadline: time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
    cancelled: &mut bool,
    child: &mut crate::child::Owner,
    mut drain: std::pin::Pin<&mut time::Sleep>,
    publish: bool,
) -> bool {
    let flush = async {
        if publish {
            collector.persist_until(deadline).await
        } else {
            collector.flush_until(deadline).await
        }
    };
    tokio::pin!(flush);
    loop {
        tokio::select! {
            result = &mut flush => return result,
            () = cancel.cancelled(), if !*cancelled => {
                *cancelled = true;
                child.kill();
                drain.as_mut().reset(time::Instant::now() + DRAIN);
            }
            () = &mut drain, if *cancelled => return false,
        }
    }
}

fn exit_outcome(status: &ExitStatus) -> JobOutcome {
    status.code().map_or_else(
        || {
            status
                .signal()
                .map_or(JobOutcome::Unknown, JobOutcome::Signal)
        },
        JobOutcome::Exit,
    )
}

pub(super) struct Completion {
    elapsed: f64,
    cancelled: bool,
    status_error: Option<String>,
}

impl Completion {
    pub(super) fn state(&self, result: &ProgramResult) -> String {
        let mut state = format!("{} after {:.1}s", result.state(), self.elapsed);
        if !result.output_file.is_complete() {
            state.push_str("; output incomplete");
        }
        if self.cancelled {
            state.push_str("; cancelled");
        }
        if let Some(details) = result.details() {
            let _ = write!(state, "; {details}");
        }
        if let Some(error) = &self.status_error {
            let _ = write!(state, "; exit status unavailable ({error})");
        }
        state
    }
}

fn termination(
    status: Option<io::Result<ExitStatus>>,
    elapsed: f64,
    cancelled: bool,
) -> (JobOutcome, Completion) {
    let outcome = if cancelled {
        JobOutcome::Killed
    } else {
        status
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .map_or(JobOutcome::Unknown, exit_outcome)
    };
    let completion = Completion {
        elapsed,
        cancelled,
        status_error: status.and_then(Result::err).map(|error| error.to_string()),
    };
    (outcome, completion)
}

const ENVIRONMENT: [(&str, &str); 4] = [
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

#[derive(Clone)]
pub(crate) struct Place {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) executable: std::path::PathBuf,
    pub(crate) spill_dir: std::path::PathBuf,
    pub(crate) confinement: Confinement,
}

impl Place {
    pub(crate) fn pycache_dir(&self) -> std::path::PathBuf {
        self.spill_dir.with_file_name("pycache")
    }
}

pub(super) fn confined(program: &str, context: &Context) -> Result<Command, String> {
    launch(program, &context.place(), &Default::default())
}

#[cfg(target_os = "linux")]
use linux::command as jailed_command;

#[cfg(target_os = "linux")]
pub(crate) fn check_jail(project: &std::path::Path) -> Result<(), String> {
    let project = project
        .canonicalize()
        .map_err(|error| crate::terminal_text::io_reason(&error))?;
    for (path, _) in linux::protected_paths()? {
        if path.starts_with(&project) || project.starts_with(&path) {
            return Err("sandbox working directory overlaps a protected path: Bart's settings or session records, or cargo credentials".into());
        }
    }
    linux::tools(&project).map(drop)
}

#[cfg(not(target_os = "linux"))]
const UNSUPPORTED: &str = "--jail needs Linux, for bubblewrap and systemd-run";

#[cfg(not(target_os = "linux"))]
pub(crate) fn check_jail(_: &std::path::Path) -> Result<(), String> {
    Err(UNSUPPORTED.into())
}

#[cfg(not(target_os = "linux"))]
fn jailed_command(
    _: &str,
    _: &Place,
    _: &std::collections::BTreeMap<String, String>,
    _: &std::path::Path,
) -> Result<Command, String> {
    Err(UNSUPPORTED.into())
}

pub(crate) fn launch(
    program: &str,
    place: &Place,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Command, String> {
    launch_in(program, place, env, &place.cwd)
}

fn launch_in(
    program: &str,
    place: &Place,
    env: &std::collections::BTreeMap<String, String>,
    cwd: &std::path::Path,
) -> Result<Command, String> {
    match std::fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "cannot enter working directory {}: Not a directory",
                cwd.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot enter working directory {}: {}",
                cwd.display(),
                crate::terminal_text::io_reason(&error)
            ));
        }
    }
    let mut command = match place.confinement {
        Confinement::Unrestricted => {
            let mut command = Command::new(&place.executable, program)
                .map_err(|error| crate::terminal_text::io_reason(&error))?;
            command.envs(ENVIRONMENT).envs(env);
            command
        }
        Confinement::Jailed(..) => jailed_command(program, place, env, cwd)?,
    };
    command.current_dir(cwd);
    Ok(command)
}

#[cfg(test)]
mod tests;
