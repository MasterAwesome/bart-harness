use rustix::process::{Pid, Signal, kill_process};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{ExitCode, Stdio},
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("process supervision supports only Linux and macOS");

pub(crate) const MODE: &str = "--bart-process-supervisor";
pub(crate) const DETACHED_MODE: &str = "--bart-detached";

pub(crate) fn detached(
    executable: &std::path::Path,
    program: impl AsRef<std::ffi::OsStr>,
) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command.arg(DETACHED_MODE).arg(program);
    command
}

pub(crate) fn detached_main() -> ExitCode {
    if let Err(error) = rustix::process::setsid() {
        crate::terminal_text::stderr_line(format_args!("bart: cannot detach process: {error}"));
        return ExitCode::FAILURE;
    }
    let mut args = std::env::args_os().skip(2);
    let Some(program) = args.next() else {
        return ExitCode::FAILURE;
    };
    let error = std::process::Command::new(program).args(args).exec();
    crate::terminal_text::stderr_line(format_args!(
        "bart: cannot execute detached process: {error}"
    ));
    ExitCode::from(127)
}
pub(crate) const FRAME_BYTES: usize = 4096;
pub(crate) const CLEANUP: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Request {
    Start {},
    Interrupt {},
    Kill {},
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Event {
    Started {},
    TargetExited { status: i32, survivors: bool },
    Empty {},
    Failed { reason: String },
}
fn report(control: &mut UnixStream, event: Event) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&event)?;
    if bytes.len() >= FRAME_BYTES {
        return Err(io::Error::other("supervisor event exceeds limit"));
    }
    bytes.push(b'\n');
    control.write_all(&bytes)
}

pub(crate) fn main() -> ExitCode {
    if rustix::process::setsid().is_err() {
        return ExitCode::FAILURE;
    }
    if run().is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
fn run() -> io::Result<()> {
    let socket = std::env::var_os("BART_SUPERVISOR_SOCKET")
        .ok_or_else(|| io::Error::other("missing control socket"))?;
    let mut control = UnixStream::connect(socket)?;
    control.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut frame = Vec::new();
    loop {
        let mut byte = [0];
        control.read_exact(&mut byte)?;
        frame.push(byte[0]);
        if frame.len() > FRAME_BYTES {
            return Err(io::Error::other("supervisor request exceeds limit"));
        }
        if byte[0] == b'\n' {
            break;
        }
    }
    if !matches!(
        serde_json::from_slice::<Request>(&frame)?,
        Request::Start {}
    ) {
        return Err(io::Error::other("expected supervisor Start"));
    }
    control.set_read_timeout(None)?;
    control.set_nonblocking(true)?;
    let tracker = sys::Tracker::new()?;
    let wake = Wake::new()?;
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let mut args = std::env::args_os().skip(2);
    let program = args
        .next()
        .ok_or_else(|| io::Error::other("missing target"))?;
    let adopt = std::env::var_os("BART_SUPERVISOR_ADOPT").is_some_and(|value| value == "1");
    let target = std::process::Command::new(program)
        .args(args)
        .env_remove("BART_SUPERVISOR_SOCKET")
        .env_remove("BART_SUPERVISOR_ADOPT")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn();
    let target = match target {
        Ok(child) => Pid::from_raw(child.id().cast_signed()).expect("positive child pid"),
        Err(error) => {
            let _ = report(
                &mut control,
                Event::Failed {
                    reason: error.to_string(),
                },
            );
            return Err(error);
        }
    };
    let mut scope = Scope {
        tracker,
        wake,
        target: Some(target),
        status: None,
        cleanup_deadline: None,
    };
    let result = (|| {
        scope.tracker.start(target)?;
        rustix::stdio::dup2_stdin(&null)?;
        rustix::stdio::dup2_stdout(&null)?;
        rustix::stdio::dup2_stderr(&null)?;
        report(&mut control, Event::Started {})?;
        scope.run(&mut control, adopt)
    })();
    if let Err(error) = result {
        let cleanup = scope.cleanup();
        let release = scope.tracker.release();
        let reason = match cleanup.and(release) {
            Ok(()) => error.to_string(),
            Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
        };
        let reason = reason[..reason.floor_char_boundary(FRAME_BYTES / 8)].to_owned();
        let _ = report(
            &mut control,
            Event::Failed {
                reason: reason.clone(),
            },
        );
        return Err(io::Error::other(reason));
    }
    Ok(())
}

struct Wake {
    signals: UnixStream,
}
impl Wake {
    fn new() -> io::Result<Self> {
        let (signals, notify) = UnixStream::pair()?;
        signals.set_nonblocking(true)?;
        notify.set_nonblocking(true)?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, notify)?;
        Ok(Self { signals })
    }
    fn drain(&self) -> io::Result<()> {
        let mut bytes = [0; 64];
        loop {
            match (&self.signals).read(&mut bytes) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
    fn wait(&self, control: Option<BorrowedFd<'_>>, timeout: Option<Duration>) -> io::Result<()> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut fds = vec![PollFd::new(&self.signals, PollFlags::IN)];
        if let Some(control) = &control {
            fds.push(PollFd::new(control, PollFlags::IN));
        }
        let timeout = timeout
            .map(Timespec::try_from)
            .transpose()
            .map_err(io::Error::other)?;
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

struct Scope {
    tracker: sys::Tracker,
    wake: Wake,
    target: Option<Pid>,
    status: Option<i32>,
    cleanup_deadline: Option<Instant>,
}
impl Scope {
    fn reap(&mut self) -> io::Result<()> {
        if let Some(status) = self.tracker.reap()? {
            self.status = Some(status);
            self.target = None;
        }
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        let deadline = *self
            .cleanup_deadline
            .get_or_insert_with(|| Instant::now() + CLEANUP);
        let mut failure = None;
        loop {
            if let Some(target) = self.target {
                let _ = kill_process(target, Signal::STOP);
            }
            let attempt = (|| {
                self.wake.drain()?;
                self.tracker.kill()?;
                if let Some(target) = self.target {
                    let _ = kill_process(target, Signal::KILL);
                }
                self.reap()?;
                self.tracker.refresh()?;
                if self.target.is_none() && !self.tracker.live()? {
                    return Ok(true);
                }
                io::Result::Ok(false)
            })();
            match attempt {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => failure = Some(error),
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(failure.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "descendants did not empty before cleanup deadline",
                    )
                }));
            }
            let left = deadline - now;
            let timeout = self
                .tracker
                .timeout()
                .map_or(left, |timeout| timeout.min(left));
            if let Err(error) = self.wake.wait(None, Some(timeout)) {
                failure = Some(error);
            }
        }
    }
    fn run(&mut self, control: &mut UnixStream, adopt: bool) -> io::Result<()> {
        let mut reported = false;
        let mut pending = Vec::new();
        loop {
            self.wake.drain()?;
            self.reap()?;
            let mut input = [0; FRAME_BYTES];
            let mut kill = false;
            loop {
                match control.read(&mut input) {
                    Ok(0) => {
                        kill = true;
                        break;
                    }
                    Ok(n) => {
                        for &byte in &input[..n] {
                            pending.push(byte);
                            if pending.len() > FRAME_BYTES {
                                return Err(io::Error::other("supervisor request exceeds limit"));
                            }
                            if byte == b'\n' {
                                match serde_json::from_slice::<Request>(&pending)? {
                                    Request::Kill {} => kill = true,
                                    Request::Interrupt {} => {
                                        // Only signal the still-unreaped target, in this namespace.
                                        if let Some(target) = self.target {
                                            let _ = kill_process(target, Signal::INT);
                                        }
                                    }
                                    Request::Start {} => {
                                        return Err(io::Error::other("duplicate supervisor Start"));
                                    }
                                }
                                pending.clear();
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            if kill || (!adopt && self.status.is_some()) {
                self.cleanup()?;
            }
            self.reap()?;
            self.tracker.refresh()?;
            let survivors = self.tracker.live()?;
            if let Some(status) = self.status
                && !reported
            {
                if let Err(error) = report(control, Event::TargetExited { status, survivors }) {
                    if !survivors && self.target.is_none() {
                        return self.tracker.release();
                    }
                    return Err(error);
                }
                reported = true;
            }
            if !survivors && self.target.is_none() {
                self.tracker.release()?;
                let _ = report(control, Event::Empty {});
                return Ok(());
            }
            self.wake
                .wait(Some(control.as_fd()), self.tracker.timeout())?;
        }
    }
}
