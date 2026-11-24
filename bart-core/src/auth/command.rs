use std::{
    collections::HashMap,
    io,
    path::Path,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{io::AsyncReadExt as _, sync::watch};

use super::{Credential, Credentials, Error, Secret, Store};
use crate::{NOT_POISONED, supervise, terminal_text};

const TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 30 });
const MAX_STDOUT: u64 = 64 * 1024;
const EXEC_DIAGNOSTIC_BYTES: u64 = 256;

#[derive(Default)]
pub(super) struct Cache(Mutex<HashMap<String, Arc<Slot>>>);

impl Cache {
    pub(super) fn retain(&self, credentials: &Credentials) {
        self.0.lock().expect(NOT_POISONED).retain(|name, slot| {
            matches!(credentials.providers.get(name), Some(Credential::Command { endpoint, argv })
                if slot.endpoint == *endpoint && slot.argv == *argv)
        });
    }

    fn slot(&self, name: &str, endpoint: &str, argv: &[String]) -> Arc<Slot> {
        let mut slots = self.0.lock().expect(NOT_POISONED);
        if let Some(slot) = slots
            .get(name)
            .filter(|slot| slot.endpoint == endpoint && slot.argv == argv)
        {
            return Arc::clone(slot);
        }
        let slot = Arc::new(Slot::new(endpoint, argv));
        slots.insert(name.to_owned(), Arc::clone(&slot));
        slot
    }
}

#[derive(Clone, Debug)]
pub(crate) enum IssueError {
    StaleBinding,
    Command(Arc<str>),
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleBinding => f.write_str("the saved provider or key command changed"),
            Self::Command(problem) => f.write_str(problem),
        }
    }
}

type Outcome = Result<Arc<Secret>, IssueError>;

enum Flight {
    Ready(Arc<Secret>),
    Run(watch::Sender<Option<Outcome>>),
    Wait(watch::Receiver<Option<Outcome>>),
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Running(watch::Receiver<Option<Outcome>>),
    Ready(Arc<Secret>),
}

struct Slot {
    endpoint: String,
    argv: Vec<String>,
    state: Mutex<State>,
}

impl Slot {
    fn new(endpoint: &str, argv: &[String]) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            argv: argv.to_vec(),
            state: Mutex::default(),
        }
    }

    async fn issue(
        self: &Arc<Self>,
        current: impl Fn(&[String]) -> bool,
    ) -> Result<Issued, IssueError> {
        let issued = |key| Issued {
            key,
            slot: Arc::clone(self),
        };
        loop {
            let flight = {
                let mut state = self.state.lock().expect(NOT_POISONED);
                match &*state {
                    State::Ready(key) => Flight::Ready(Arc::clone(key)),
                    State::Running(receiver) if !receiver.is_closed() => {
                        Flight::Wait(receiver.clone())
                    }
                    _ => {
                        let (sender, receiver) = watch::channel(None);
                        *state = State::Running(receiver);
                        Flight::Run(sender)
                    }
                }
            };
            let outcome = match flight {
                Flight::Ready(key) => Ok(key),
                Flight::Run(sender) => {
                    let outcome = if current(&self.argv) {
                        let result = run(&self.argv).await;
                        if current(&self.argv) {
                            result
                                .map(Arc::new)
                                .map_err(|error| IssueError::Command(error.into()))
                        } else {
                            Err(IssueError::StaleBinding)
                        }
                    } else {
                        Err(IssueError::StaleBinding)
                    };
                    let mut state = self.state.lock().expect(NOT_POISONED);
                    *state = match &outcome {
                        Ok(key) => State::Ready(Arc::clone(key)),
                        Err(_) => State::Idle,
                    };
                    sender.send_replace(Some(outcome.clone()));
                    return outcome.map(issued);
                }
                Flight::Wait(mut receiver) => {
                    let Ok(outcome) = receiver.wait_for(Option::is_some).await else {
                        continue;
                    };
                    outcome.as_ref().expect("completed flight").clone()
                }
            };
            if !current(&self.argv) {
                return Err(IssueError::StaleBinding);
            }
            return outcome.map(issued);
        }
    }
}

pub(crate) struct Issued {
    key: Arc<Secret>,
    slot: Arc<Slot>,
}

impl Issued {
    pub(crate) fn key(&self) -> &Secret {
        &self.key
    }

    pub(crate) fn reject(&self) {
        let mut state = self.slot.state.lock().expect(NOT_POISONED);
        if matches!(&*state, State::Ready(key) if Arc::ptr_eq(key, &self.key)) {
            *state = State::Idle;
        }
    }

    pub(crate) async fn refresh(
        &self,
        current: impl Fn(&[String]) -> bool,
    ) -> Result<Self, IssueError> {
        self.slot.issue(current).await
    }
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Issued(<redacted>)")
    }
}

pub async fn verify(argv: &[String]) -> Result<(), Error> {
    run(argv).await.map(drop).map_err(Error::KeyCommand)
}

pub(crate) async fn resolve(
    store: &Store,
    name: &str,
    endpoint: &str,
    argv: &[String],
    current: impl Fn(&[String]) -> bool,
) -> Result<Issued, IssueError> {
    store
        .commands
        .slot(name, endpoint, argv)
        .issue(current)
        .await
}

fn validate(argv: &[String]) -> Result<(&str, &[String]), String> {
    let Some((program, arguments)) = argv
        .split_first()
        .filter(|(program, _)| !program.is_empty())
    else {
        return Err("a key command needs a program".into());
    };
    if argv.iter().any(|arg| arg.contains('\0')) {
        return Err("a key command cannot contain NUL bytes".into());
    }
    if program.contains('/') && !Path::new(program).is_absolute() {
        return Err(
            "a key command's program must be a name found on PATH or an absolute path".into(),
        );
    }
    Ok((program, arguments))
}

struct Group(tokio::process::Child);

impl Drop for Group {
    fn drop(&mut self) {
        supervise::kill_group(self.0.id());
    }
}

async fn run(argv: &[String]) -> Result<Secret, String> {
    let (program, arguments) = validate(argv)?;
    let shown = terminal_text::printable(program, &[]);
    let failed = |problem: &str| format!("key command `{shown}` {problem}");
    #[cfg(not(test))]
    let launcher =
        supervise::detached(program).map_err(|error| failed(&format!("cannot start: {error}")))?;
    #[cfg(test)]
    let launcher = supervise::detached_via(crate::tool::test_support::executable(), program);
    let mut command = tokio::process::Command::from(launcher);
    let mut child = Group(
        command
            .args(arguments)
            .current_dir(
                std::env::var_os("HOME")
                    .filter(|path| Path::new(path).is_absolute())
                    .unwrap_or_else(|| "/".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| failed(&format!("cannot start: {error}")))?,
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let stderr = child.0.stderr.take().expect("piped stderr");
    let finished = async {
        let output = async {
            let mut output = Vec::new();
            stdout
                .take(MAX_STDOUT + 1)
                .read_to_end(&mut output)
                .await
                .map_err(|error| failed(&format!("cannot read stdout: {error}")))?;
            if output.len() as u64 > MAX_STDOUT {
                return Err(failed("printed more than 64 KiB"));
            }
            Ok(output)
        };
        let errors = async {
            stderr_prefix(stderr)
                .await
                .map_err(|error| failed(&format!("cannot read stderr: {error}")))
        };
        let (output, errors) = tokio::try_join!(output, errors)?;
        let status = child
            .0
            .wait()
            .await
            .map_err(|error| failed(&format!("cannot wait: {error}")))?;
        Ok::<_, String>((status, output, errors))
    };
    let (status, output, errors) = tokio::time::timeout(TIMEOUT, finished)
        .await
        .map_err(|_| failed(&format!("timed out after {}s", TIMEOUT.as_secs())))??;
    if !status.success() {
        return Err(failed(&exit_problem(status, &errors)));
    }
    let output = String::from_utf8_lossy(&output);
    super::api_key(&output).map_err(|_| {
        failed(if output.trim_ascii().is_empty() {
            "printed no key"
        } else {
            "did not print one token of printable ASCII of at most 4096 bytes"
        })
    })
}

async fn stderr_prefix(mut stderr: tokio::process::ChildStderr) -> io::Result<Vec<u8>> {
    let mut prefix = Vec::new();
    (&mut stderr)
        .take(EXEC_DIAGNOSTIC_BYTES)
        .read_to_end(&mut prefix)
        .await?;
    if tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await? != 0 {
        prefix.clear();
    }
    Ok(prefix)
}

fn exit_problem(status: ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    if status.code() == Some(127)
        && let Some(reason) = stderr
            .trim_ascii()
            .strip_prefix(supervise::DETACHED_EXEC_FAILED)
    {
        use rustix::io::Errno;
        for (errno, label) in [
            (Errno::NOENT, "not found"),
            (Errno::ACCESS, "permission denied"),
            (Errno::NOEXEC, "invalid executable format"),
        ] {
            if reason == io::Error::from(errno).to_string() {
                return format!("cannot run: {label}");
            }
        }
    }
    status.code().map_or_else(
        || "was terminated by a signal".into(),
        |code| format!("exited with status {code}"),
    )
}

#[cfg(test)]
mod tests;
