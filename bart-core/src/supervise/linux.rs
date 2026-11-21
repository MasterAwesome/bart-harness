use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, WaitOptions, kill_process, wait, waitid,
};
use std::{io, time::Duration};

pub(super) struct Tracker {
    target: Option<Pid>,
    retry: bool,
}
impl Tracker {
    pub(super) fn new() -> io::Result<Self> {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
        Ok(Self {
            target: None,
            retry: false,
        })
    }
    pub(super) fn start(&mut self, target: Pid) -> io::Result<()> {
        self.target = Some(target);
        Ok(())
    }
    pub(super) fn reap(&mut self) -> io::Result<Option<i32>> {
        let mut exited = None;
        loop {
            match wait(WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    if self.target == Some(pid) {
                        exited = Some(status.as_raw());
                        self.target = None;
                    }
                }
                Ok(None) | Err(rustix::io::Errno::CHILD) => return Ok(exited),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    pub(super) fn refresh(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn children(&self) -> io::Result<Vec<Pid>> {
        let pid = std::process::id();
        let text = std::fs::read_to_string(format!("/proc/self/task/{pid}/children"))?;
        text.split_whitespace()
            .map(|pid| {
                pid.parse::<i32>()
                    .ok()
                    .and_then(Pid::from_raw)
                    .ok_or_else(|| io::Error::other("invalid proc child pid"))
            })
            .collect()
    }
    pub(super) fn live(&self) -> io::Result<bool> {
        loop {
            match waitid(
                WaitId::All,
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            ) {
                Err(rustix::io::Errno::CHILD) => return Ok(false),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
                Ok(_) => return Ok(true),
            }
        }
    }
    pub(super) fn timeout(&self) -> Option<Duration> {
        self.retry.then_some(Duration::from_millis(1))
    }
    pub(super) fn kill(&mut self) -> io::Result<()> {
        // Direct children cannot have their IDs reused before we reap them.
        // Killing a parent reparents its children here; repeat after reaping.
        let children = self.children()?;
        self.retry = children.is_empty();
        for &pid in &children {
            let _ = kill_process(pid, Signal::STOP);
        }
        for pid in children {
            let _ = kill_process(pid, Signal::KILL);
        }
        Ok(())
    }
    pub(super) fn release(&mut self) -> io::Result<()> {
        Ok(())
    }
}
