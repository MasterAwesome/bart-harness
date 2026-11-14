use std::{ops::ControlFlow, process::Stdio, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, BufReader},
    process::ChildStderr,
    sync::{Mutex, OwnedMutexGuard},
    time,
};

use super::{
    CANCELLED, CellReview, INTERRUPTED, MAX_FRAME_BYTES,
    capture::Capture,
    helper_configuration,
    helpers::{Prepared, Work, finish, prepare},
    jsonl,
    protocol::HostMessage,
    transport::{CellOutput, Channel, Exchange, Transition, transition},
};
use crate::tool::{Context, process::confined};

const HARNESS: &str = include_str!("harness.py");

pub(super) const INTERRUPT_GRACE: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(in crate::tool) struct Interpreter {
    pub(super) slot: Arc<Mutex<Slot>>,
    pub(super) stopped: Arc<std::sync::atomic::AtomicBool>,
    pub(super) reset: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
pub(super) struct Slot {
    pub(super) running: Option<Running>,
    pub(super) notice: Option<&'static str>,
    pub(super) cancelled: Option<(Capture, End)>,
}

pub(super) const LOST: &str = "interpreter had stopped; earlier Python state is gone";
pub(super) const REWOUND: &str =
    "interpreter restarted after a rewind; earlier Python state is gone";

impl Interpreter {
    pub(in crate::tool) fn inventory(&self) -> String {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire)
            || self.reset.load(std::sync::atomic::Ordering::Acquire)
        {
            return "Python inventory unavailable: interpreter stopped or reset.".into();
        }
        let Ok(mut slot) = self.slot.try_lock() else {
            return "Python inventory unavailable: a cell is running.".into();
        };
        let Some(running) = &mut slot.running else {
            return "Python inventory unavailable: no live interpreter.".into();
        };
        if running.busy.is_some() {
            return "Python inventory unavailable: a cell is still running.".into();
        }
        match &running.channel.inventory {
            Some(value) => format!(
                "Python namespace at the last settled cell {:?} (not restored state; background mutations may occur; null function source means its defining cell was not tracked): {value}",
                running.inventory_cell
            ),
            None => "Python inventory unavailable: no settled cell.".into(),
        }
    }

    pub(in crate::tool) async fn restart(&self, context: &Context) -> Result<(), String> {
        let mut slot = self.slot.lock().await;
        slot.cancelled.take();
        slot.running = Some(start_cell(&mut slot, true, context).await?.0);
        Ok(())
    }

    pub(in crate::tool) fn reset(&self) {
        self.reset.store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut slot) = self.slot.try_lock() {
            self.apply_reset(&mut slot);
        }
    }

    pub(super) fn apply_reset(&self, slot: &mut Slot) {
        if self.reset.swap(false, std::sync::atomic::Ordering::AcqRel) {
            let running = slot.running.take().is_some();
            let cancelled = slot.cancelled.take().is_some();
            if running || cancelled || slot.notice.is_some() {
                slot.notice = Some(REWOUND);
            }
        }
    }

    pub(in crate::tool) fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut slot) = self.slot.try_lock() {
            slot.running.take();
            slot.cancelled.take();
        }
    }
}

pub(super) struct Lease {
    pub(super) stopped: Arc<std::sync::atomic::AtomicBool>,
    pub(super) slot: Option<OwnedMutexGuard<Slot>>,
    pub(super) running: Option<Running>,
    pub(super) capture: Option<Capture>,
}

impl Lease {
    pub(super) fn finish(mut self, end: &End) -> Capture {
        let capture = self.capture.take().expect("leased capture");
        let mut running = self.running.take();
        if let Some(running) = &mut running {
            running.busy = busy(end, &capture);
            if matches!(end, End::Done { .. } | End::Cancelled) {
                running.inventory_cell = capture.collector.cell();
            }
        }
        if let Some(slot) = &mut self.slot {
            slot.running = running.filter(|_| {
                !self.stopped.load(std::sync::atomic::Ordering::Acquire)
                    && matches!(end, End::Done { .. } | End::Cancelled | End::TimedOut)
            });
        }
        capture
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let (Some(mut running), Some(mut slot), Some(mut capture)) =
            (self.running.take(), self.slot.take(), self.capture.take())
        else {
            return;
        };
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let stopped = Arc::clone(&self.stopped);
            runtime.spawn(async move {
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                capture.cancelled = true;
                let end = running.settle(&mut capture).await;
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                running.busy = busy(&end, &capture);
                if matches!(end, End::Done { .. } | End::Cancelled) {
                    running.inventory_cell = capture.collector.cell();
                }
                let keep = matches!(end, End::Done { .. } | End::Cancelled | End::TimedOut);
                slot.running = keep.then_some(running);
                if !keep {
                    slot.notice = Some(LOST);
                }
                slot.cancelled = Some((capture, end));
            });
        }
    }
}

pub(super) struct Running {
    pub(super) child: crate::child::Owner,
    pub(super) channel: Channel,
    pub(super) output: ChildStderr,
    pub(super) emission: CellOutput,
    pub(super) busy: Option<Busy>,
    inventory_cell: Option<u64>,
}

#[derive(Clone)]
pub(super) struct Busy {
    pub(super) cell: u64,
    pub(super) processes: u32,
    pub(super) saved: crate::history::SavedArtifact,
}

fn busy(end: &End, capture: &Capture) -> Option<Busy> {
    matches!(end, End::TimedOut)
        .then(|| capture.collector.cell())
        .flatten()
        .map(|cell| Busy {
            cell,
            processes: capture.effects.processes,
            saved: capture.collector.saved(),
        })
}

pub(super) enum End {
    Done { failed: bool, interrupted: bool },
    Cancelled,
    TimedOut,
    Exited,
    Broken(std::io::Error),
    Discarded,
}

pub(super) async fn start_cell(
    slot: &mut Slot,
    restart: bool,
    context: &Context,
) -> Result<(Running, Option<&'static str>), String> {
    let mut note = slot.notice.take();
    let mut running = slot.running.take();
    if restart && running.take().is_some() {
        note = Some("interpreter restarted as asked; earlier Python state is gone");
    }
    let running = match running {
        Some(running) => running,
        None => Running::start(context).await?,
    };
    Ok((running, note))
}

impl Running {
    pub(super) async fn start(context: &Context) -> Result<Self, String> {
        let failed = |error: std::io::Error| {
            format!(
                "cannot start python3: {}",
                crate::terminal_text::io_reason(&error)
            )
        };
        let random = crate::random::bytes::<16>().map_err(failed)?;
        let marker = format!("bart-cell-end-{:032x}", u128::from_ne_bytes(random));
        let pycache = context.pycache_dir();
        match std::fs::create_dir(&pycache) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(failed(error)),
        }
        let mut command = confined("python3", context)?;
        command
            .args([
                "-I",
                "-u",
                "-c",
                "import sys; exec(compile(sys.argv.pop(1), '<bart-harness>', 'exec'))",
                HARNESS,
                &marker,
            ])
            .arg(&pycache)
            .arg(helper_configuration(context).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::child::Owner::spawn(&mut command)
            .await
            .map_err(failed)?;
        let (Some(requests), Some(replies), Some(output)) = child.pipes() else {
            return Err("cannot start python3: its pipes are missing".into());
        };
        Ok(Self {
            child,
            channel: Channel {
                requests,
                replies: jsonl::Reader::new(BufReader::new(replies)),
                exchange: Exchange::default(),
                process: None,
                inventory: None,
            },
            output,
            emission: CellOutput {
                marker: [b"\n", marker.as_bytes(), b"\n"].concat(),
                barrier: [b"\0", marker.as_bytes(), b":remaining\0"].concat(),
                synchronized: 0,
                pending: Vec::new(),
                output: Vec::new(),
                ended: false,
            },
            busy: None,
            inventory_cell: None,
        })
    }

    fn prepare_cell(&mut self, cell: u64, code: Option<&str>, web_search: bool, diffs: bool) {
        if let Some(code) = code {
            self.emission.output = std::mem::take(&mut self.emission.pending);
            self.emission.ended = false;
            self.channel.exchange = Exchange::default();
            self.channel.exchange.queue(&HostMessage::Run {
                cell,
                code,
                web_search,
                diffs,
            });
        } else if self.channel.exchange.awaiting && self.channel.process.is_none() {
            self.channel
                .exchange
                .queue(&HostMessage::Error(INTERRUPTED));
        }
    }

    pub(super) async fn cell(
        &mut self,
        code: Option<&str>,
        deadline: time::Instant,
        capture: &mut Capture,
        review: &mut CellReview<'_, '_>,
    ) -> End {
        let cancelled = capture.cancelled;
        let web_search = review.context.web_route().is_some();
        let cell = capture
            .collector
            .cell()
            .expect("a cell's collector holds its output");
        self.prepare_cell(
            cell,
            code,
            web_search,
            review
                .context
                .diffs
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        let mut timer = CellTimer::new(deadline);
        match time::timeout_at(timer.deadline, self.channel.send()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return End::Broken(error),
            Err(_) => return End::TimedOut,
        }
        let mut buffer = vec![0; 64 * 1024];
        let end = loop {
            match flush_output(
                &self.child,
                &mut self.emission,
                &mut capture.collector,
                &mut capture.stream,
                &mut timer,
            )
            .await
            {
                Ok(None) => {}
                Ok(Some(end)) => break end,
                Err(error) => break End::Broken(error),
            }
            if let (Some(failed), true) = (self.channel.exchange.done, self.emission.ended) {
                break if cancelled {
                    End::Cancelled
                } else {
                    End::Done {
                        failed,
                        interrupted: timer.interrupted,
                    }
                };
            }
            let step = tokio::select! {
                read = self.output.read(&mut buffer), if !self.emission.ended => read.map(|n| {
                    self.emission.output.extend_from_slice(&buffer[..n]);
                    (n == 0).then_some(End::Exited)
                }),
                read = async {
                    if self.channel.process.is_some() { Ok(1) }
                    else { self.channel.replies.read(MAX_FRAME_BYTES).await }
                }, if self.channel.exchange.done.is_none() => {
                    let room = if let Ok(super::protocol::PythonMessage::Call(super::protocol::Helper::Remaining { barrier, .. })) =
                        serde_json::from_slice(&self.channel.replies.frame) {
                        while self.emission.synchronized < barrier {
                            let n = match time::timeout_at(timer.deadline, self.output.read(&mut buffer)).await {
                                Ok(Ok(n)) => n,
                                Ok(Err(error)) => return End::Broken(error),
                                Err(_) => return End::TimedOut,
                            };
                            if n == 0 { return End::Exited; }
                            self.emission.output.extend_from_slice(&buffer[..n]);
                            self.emission.capture(&mut capture.collector, &mut capture.stream);
                        }
                        capture.collector.room().saturating_sub(capture.stream.pending_bytes())
                    } else { 0 };
                    let helper_live = capture.stream.progress();
                    let prepared = drive_helper(
                        prepare(
                            read,
                            &mut self.channel,
                            &mut capture.effects,
                            cell,
                            review,
                            &helper_live,
                            room,
                        ),
                        &self.child,
                        (&mut self.output, &mut buffer),
                        &mut self.emission,
                        &mut capture.collector,
                        &mut capture.stream,
                        &mut timer,
                    ).await;
                    match prepared {
                        Ok(ControlFlow::Continue(Prepared::Handled)) => Ok(None),
                        Ok(ControlFlow::Break(end) | ControlFlow::Continue(Prepared::End(end))) => Ok(Some(end)),
                        Ok(ControlFlow::Continue(Prepared::Work(work))) => {
                            self.run_work(work, capture, review, &mut buffer, &mut timer)
                                .await
                        }
                        Err(error) => Err(error),
                    }
                }
                () = time::sleep_until(timer.deadline) => {
                    timer.overdue(&self.child).map(|ended| ended.then_some(End::TimedOut))
                }
            };
            match step {
                Ok(None) => {}
                Ok(Some(end)) => break end,
                Err(error) => break End::Broken(error),
            }
        };
        if !matches!(end, End::Done { .. } | End::Cancelled) {
            let rest = std::mem::take(&mut self.emission.output);
            capture
                .stream
                .render(&rest, |line| capture.collector.line(line));
        }
        capture.stream.finish(|line| capture.collector.line(line));
        end
    }

    async fn run_work(
        &mut self,
        work: Work,
        capture: &mut Capture,
        review: &mut CellReview<'_, '_>,
        buffer: &mut [u8],
        timer: &mut CellTimer,
    ) -> std::io::Result<Option<End>> {
        let saved_remaining = timer.pause(work.watchdog(&self.channel));
        let helper_live = capture.stream.progress();
        let result = drive_helper(
            finish(
                work,
                &mut self.channel,
                &mut capture.effects,
                review,
                &helper_live,
            ),
            &self.child,
            (&mut self.output, buffer),
            &mut self.emission,
            &mut capture.collector,
            &mut capture.stream,
            timer,
        )
        .await;
        timer.resume(saved_remaining);
        result.map(|flow| match flow {
            ControlFlow::Break(end) => Some(end),
            ControlFlow::Continue(end) => end,
        })
    }

    pub(super) async fn settle(&mut self, capture: &mut Capture) -> End {
        if let Err(error) = self.child.interrupt() {
            return End::Broken(error);
        }
        self.channel.process.take();
        self.channel.exchange.cancelled = true;
        let Capture {
            collector,
            effects,
            stream,
            ..
        } = capture;
        let settled = time::timeout(INTERRUPT_GRACE, async {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                if self.channel.exchange.awaiting {
                    self.channel.exchange.queue(&HostMessage::Error(CANCELLED));
                }
                self.channel.send().await?;
                self.emission.capture(collector, stream);
                collector.flush().await;
                if self.channel.exchange.done.is_some() && self.emission.ended {
                    return Ok(End::Cancelled);
                }
                tokio::select! {
                    read = self.output.read(&mut buffer), if !self.emission.ended => {
                        let n = read?;
                        if n == 0 { return Ok(End::Exited); }
                        self.emission.output.extend_from_slice(&buffer[..n]);
                    }
                    read = self.channel.replies.read(MAX_FRAME_BYTES), if self.channel.exchange.done.is_none() => {
                        match transition(read, &mut self.channel, effects).await? {
                            Transition::End(end) => return Ok(end),
                            Transition::Call(..) | Transition::Activity(..) => self.channel.exchange.queue(&HostMessage::Error(CANCELLED)),
                            Transition::Continue => {}
                        }
                    }
                }
            }
        }).await;
        match settled {
            Ok(Ok(end)) => end,
            Ok(Err(error)) => End::Broken(error),
            Err(_) => End::TimedOut,
        }
    }
}

async fn drive_helper<T>(
    helper: impl Future<Output = std::io::Result<T>>,
    child: &crate::child::Owner,
    (output, buffer): (&mut ChildStderr, &mut [u8]),
    emission: &mut CellOutput,
    collector: &mut crate::tool::output::Collector,
    stream: &mut crate::tool::live::Stream,
    timer: &mut CellTimer,
) -> std::io::Result<ControlFlow<End, T>> {
    tokio::pin!(helper);
    loop {
        tokio::select! {
            result = &mut helper => return result.map(ControlFlow::Continue),
            read = output.read(buffer), if !emission.ended => {
                let n = read?;
                if n == 0 {
                    return Ok(ControlFlow::Break(End::Exited));
                }
                emission.output.extend_from_slice(&buffer[..n]);
                if let Some(end) = flush_output(child, emission, collector, stream, timer).await? {
                    return Ok(ControlFlow::Break(end));
                }
            }
            () = time::sleep_until(timer.deadline) => {
                if timer.overdue(child)? {
                    return Ok(ControlFlow::Break(End::TimedOut));
                }
            }
        }
    }
}

struct CellTimer {
    deadline: time::Instant,
    interrupted: bool,
}

async fn flush_output(
    child: &crate::child::Owner,
    emission: &mut CellOutput,
    collector: &mut crate::tool::output::Collector,
    stream: &mut crate::tool::live::Stream,
    timer: &mut CellTimer,
) -> std::io::Result<Option<End>> {
    emission.capture(collector, stream);
    if !collector.flush_until(timer.deadline).await && timer.overdue(child)? {
        return Ok(Some(End::TimedOut));
    }
    Ok(None)
}

impl CellTimer {
    fn new(deadline: time::Instant) -> Self {
        Self {
            deadline,
            interrupted: false,
        }
    }

    fn overdue(&mut self, child: &crate::child::Owner) -> std::io::Result<bool> {
        if self.interrupted {
            return Ok(true);
        }
        child.interrupt()?;
        self.interrupted = true;
        self.deadline = time::Instant::now() + INTERRUPT_GRACE;
        Ok(false)
    }

    fn pause(&mut self, watchdog: Option<Duration>) -> Option<Duration> {
        if self.interrupted {
            return None;
        }
        let watchdog = watchdog?;
        let remaining = self
            .deadline
            .saturating_duration_since(time::Instant::now());
        self.deadline = time::Instant::now() + watchdog;
        Some(remaining)
    }

    fn resume(&mut self, saved_remaining: Option<Duration>) {
        if !self.interrupted
            && let Some(remaining) = saved_remaining
        {
            self.deadline = time::Instant::now() + remaining;
        }
    }
}

pub(in crate::tool) async fn identity(context: &Context) -> Option<String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        version: String,
        executable: std::path::PathBuf,
    }
    let mut command = confined("python3", context).ok()?;
    command
        .args([
            "-I",
            "-c",
            "import json, sys; print(json.dumps({'version': '%d.%d' % sys.version_info[:2], 'executable': sys.executable}))",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::child::Owner::spawn(&mut command).await.ok()?;
    let (_, Some(stdout), _) = child.pipes() else {
        return None;
    };
    let answer = async {
        let mut text = String::new();
        stdout.take(4096).read_to_string(&mut text).await.ok()?;
        child.wait().await.ok()?.success().then_some(text)
    };
    let text = time::timeout(Duration::from_secs(10), answer)
        .await
        .ok()??;
    let answer: Answer = serde_json::from_str(text.trim()).ok()?;
    let numeric = answer
        .version
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    (numeric && answer.executable.is_absolute()).then(|| {
        format!(
            "Python {} ({})",
            answer.version,
            answer.executable.display()
        )
    })
}
