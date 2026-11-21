use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use std::{io, process::ExitStatus};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

pub(super) struct Owner {
    child: Child,
    pid: Option<Pid>,
}
impl Owner {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command.kill_on_drop(true).spawn()?;
        let pid = child.id().and_then(|pid| Pid::from_raw(pid.cast_signed()));
        Ok(Self { child, pid })
    }
    pub(super) fn pipes(
        &mut self,
    ) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
        (
            self.child.stdin.take(),
            self.child.stdout.take(),
            self.child.stderr.take(),
        )
    }
    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(pid) = self.pid {
            loop {
                match waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(_)) => break,
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            // The unreaped leader still pins the group ID.
            self.kill();
        }
        self.child.wait().await
    }
    fn kill(&mut self) {
        if let Some(pid) = self.pid.take() {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.kill();
    }
}
