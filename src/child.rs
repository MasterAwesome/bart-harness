use crate::supervise::{CLEANUP, Event, FRAME_BYTES, Request};
use std::{
    io,
    ops::{Deref, DerefMut},
    os::unix::{fs::DirBuilderExt, net::UnixListener},
    path::{Path, PathBuf},
    process::ExitStatus,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

pub(crate) struct Prepared {
    command: Command,
    listener: UnixListener,
    directory: PathBuf,
    retain_descendants: bool,
    #[cfg(target_os = "linux")]
    argument_file: Option<std::fs::File>,
}
impl Prepared {
    pub(crate) fn new(executable: &Path, program: impl AsRef<std::ffi::OsStr>) -> io::Result<Self> {
        let mut command = Command::new(executable);
        command.arg(crate::supervise::MODE).arg(program);
        Self::from_command(command)
    }
    pub(crate) fn from_command(command: Command) -> io::Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "bart-{:032x}",
            u128::from_ne_bytes(crate::random::bytes()?)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let listener = match UnixListener::bind(directory.join("control")) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(error);
            }
        };
        listener.set_nonblocking(true)?;
        Ok(Self {
            command,
            listener,
            directory,
            retain_descendants: false,
            #[cfg(target_os = "linux")]
            argument_file: None,
        })
    }
    pub(crate) fn retain_descendants(&mut self) {
        self.retain_descendants = true;
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn argument_file(&mut self, file: std::fs::File) {
        self.argument_file = Some(file);
    }

    pub(crate) fn socket(&self) -> PathBuf {
        self.directory.join("control")
    }
}
impl Deref for Prepared {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.command
    }
}
impl DerefMut for Prepared {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.socket());
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TargetExit {
    pub(crate) status: ExitStatus,
    pub(crate) survivors: bool,
}
pub(crate) struct Empty;

pub(crate) struct Owner {
    child: Option<Child>,
    control: Option<BufReader<UnixStream>>,
    started: bool,
    target: Option<TargetExit>,
    empty: bool,
    task: Option<tokio::task::JoinHandle<()>>,
    frame: Vec<u8>,
    failure: Option<String>,
}
impl Owner {
    pub(crate) async fn spawn(command: &mut Prepared) -> io::Result<Self> {
        let socket = command.socket();
        let retain = if command.retain_descendants { "1" } else { "0" };
        command
            .env("BART_SUPERVISOR_SOCKET", socket)
            .env("BART_SUPERVISOR_ADOPT", retain);
        let listener = tokio::net::UnixListener::from_std(command.listener.try_clone()?)?;
        let mut owner = Self {
            child: Some(command.spawn()?),
            control: None,
            started: false,
            target: None,
            empty: false,
            task: None,
            frame: Vec::new(),
            failure: None,
        };
        let start = async {
            let child = owner.child.as_mut().expect("starting supervisor");
            let (socket, _) = tokio::select! {
                connected = listener.accept() => connected?,
                exited = async {
                    loop {
                        if let Some(status) = child.try_wait()? { break io::Result::Ok(status); }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                } => {
                    let status = exited?;
                    let mut diagnostic = Vec::new();
                    if let Some(stderr) = child.stderr.take() {
                        use tokio::io::AsyncReadExt as _;
                        let _ = tokio::time::timeout(Duration::from_millis(100), stderr.take(FRAME_BYTES as u64).read_to_end(&mut diagnostic)).await;
                    }
                    owner.child = None;
                    return Err(io::Error::other(format!("supervisor launch exited: {status}: {}", crate::terminal_text::safe_text(&String::from_utf8_lossy(&diagnostic), crate::terminal_text::TextMode::Diagnostic))));
                }
            };
            owner.control = Some(BufReader::new(socket));
            owner.request(Request::Start {})?;
            owner.event().await?;
            if !owner.started {
                return Err(io::Error::other("supervisor did not start target"));
            }
            io::Result::Ok(())
        };
        tokio::time::timeout(Duration::from_secs(10), start)
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "supervisor startup deadline exceeded",
                )
            })??;
        Ok(owner)
    }
    pub(crate) fn pipes(
        &mut self,
    ) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
        let child = self.child.as_mut().expect("owned supervisor");
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    }
    pub(crate) fn task(&mut self, task: tokio::task::JoinHandle<()>) {
        self.task = Some(task);
    }
    fn request(&self, request: Request) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        let control = self
            .control
            .as_ref()
            .ok_or_else(|| io::Error::other("supervisor not connected"))?;
        match control.get_ref().try_write(&bytes) {
            Ok(n) if n == bytes.len() => Ok(()),
            Ok(_) => Err(io::Error::other("incomplete supervisor request")),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn interrupt(&self) -> io::Result<()> {
        self.request(Request::Interrupt {})
    }
    pub(crate) fn kill(&self) {
        if let Some(control) = &self.control {
            let _ = self.request(Request::Kill {});
            let _ = rustix::net::shutdown(control.get_ref(), rustix::net::Shutdown::Write);
        }
    }
    async fn read_event(&mut self) -> io::Result<Event> {
        let control = self.control.as_mut().expect("connected supervisor");
        loop {
            let available = control.fill_buf().await?;
            if available.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "supervisor disconnected",
                ));
            }
            let n = available
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(available.len(), |n| n + 1);
            if self.frame.len() + n > FRAME_BYTES {
                return Err(io::Error::other("supervisor frame exceeds limit"));
            }
            self.frame.extend_from_slice(&available[..n]);
            control.consume(n);
            if self.frame.last() == Some(&b'\n') {
                let event = serde_json::from_slice(&self.frame)?;
                self.frame.clear();
                return Ok(event);
            }
        }
    }
    async fn event(&mut self) -> io::Result<()> {
        use std::os::unix::process::ExitStatusExt;
        let result = async {
            if let Some(error) = &self.failure {
                return Err(io::Error::other(error.clone()));
            }
            match self.read_event().await? {
                Event::Started {} if !self.started && self.target.is_none() && !self.empty => {
                    self.started = true
                }
                Event::TargetExited { status, survivors }
                    if self.started && self.target.is_none() && !self.empty =>
                {
                    let exit = ExitStatus::from_raw(status);
                    let valid = (status & !0xff00 == 0)
                        || (status & !0xff == 0 && status & 0x7f > 0 && status & 0x7f < 0x7f);
                    if !valid || (exit.code().is_none() && exit.signal().is_none()) {
                        return Err(io::Error::other("invalid target exit status"));
                    }
                    self.target = Some(TargetExit {
                        status: exit,
                        survivors,
                    });
                }
                Event::Empty {} if self.target.is_some() && !self.empty => self.empty = true,
                Event::Failed { reason } => {
                    let phase = if self.started {
                        "supervisor cleanup failed"
                    } else {
                        "supervisor failed before program start"
                    };
                    return Err(io::Error::other(format!("{phase}: {reason}")));
                }
                _ => return Err(io::Error::other("invalid supervisor event ordering")),
            }
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            self.failure = Some(error.to_string());
            self.kill();
            if let Err(cleanup) = self.reap().await {
                return Err(io::Error::other(format!(
                    "{}; {cleanup}",
                    result.expect_err("failed protocol")
                )));
            }
        }
        result
    }
    pub(crate) async fn target_exit(&mut self) -> io::Result<TargetExit> {
        while self.target.is_none() {
            self.event().await?;
        }
        Ok(self.target.expect("target exited"))
    }
    async fn reap(&mut self) -> io::Result<()> {
        if let Some(child) = &mut self.child {
            let status = tokio::time::timeout(CLEANUP + Duration::from_secs(1), child.wait())
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "supervisor cleanup deadline exceeded",
                    )
                })??;
            self.child = None;
            if !status.success() {
                return Err(io::Error::other(format!("supervisor exited: {status}")));
            }
        }
        self.child = None;
        Ok(())
    }
    pub(crate) async fn empty(&mut self) -> io::Result<Empty> {
        while !self.empty {
            self.event().await?;
        }
        self.reap().await?;
        Ok(Empty)
    }
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let exit = self.target_exit().await?;
        self.empty().await?;
        Ok(exit.status)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        self.kill();
        let connected = self.control.take().is_some();
        if let Some(mut child) = self.child.take() {
            if !connected {
                let _ = child.start_kill();
            }
            let deadline = Instant::now() + CLEANUP + Duration::from_secs(1);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() {
                            crate::terminal_text::stderr_line(format_args!(
                                "bart: supervisor failed during cleanup: {status}"
                            ));
                        }
                        break;
                    }
                    Err(error) => {
                        crate::terminal_text::stderr_line(format_args!(
                            "bart: cannot reap supervisor: {error}"
                        ));
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Ok(None) => {
                        crate::terminal_text::stderr_line(format_args!(
                            "bart: supervisor cleanup deadline exceeded"
                        ));
                        // Do not kill the ancestor which is still reclaiming its descendants.
                        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                            runtime.spawn(async move {
                                let _ = child.wait().await;
                            });
                        }
                        break;
                    }
                }
            }
        }
    }
}
