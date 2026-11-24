use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use std::{io, process::ExitStatus};
use tokio::{
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::oneshot,
};

#[derive(Clone, Copy)]
pub(super) enum Descendants {
    Kill,
    KeepOnSuccess,
}

pub(super) struct Owner {
    child: Child,
    pid: Option<Pid>,
}
impl Owner {
    pub(super) async fn spawn(mut command: Command) -> io::Result<Self> {
        let (send, receive) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            if !send.is_closed() {
                // If the caller cancels, drop the owner here to stop the helper.
                let _ = send.send(Self::launch(&mut command));
            }
        });
        receive.await.map_err(io::Error::other)?
    }

    fn launch(command: &mut Command) -> io::Result<Self> {
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
    pub(super) async fn wait(&mut self, descendants: Descendants) -> io::Result<ExitStatus> {
        if let Some(pid) = self.pid {
            loop {
                match waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(status)) => {
                        if matches!(descendants, Descendants::KeepOnSuccess)
                            && status.exit_status() == Some(0)
                        {
                            self.pid = None;
                        } else {
                            // The unreaped leader still pins the group ID.
                            self.kill();
                        }
                        break;
                    }
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
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
