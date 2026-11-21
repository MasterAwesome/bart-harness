use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, WaitIdStatus, WaitOptions, kill_process,
    kill_process_group, waitid, waitpid,
};
use std::{
    io,
    time::{Duration, Instant},
};

const QUERY: Duration = Duration::from_millis(100);

pub(super) struct Tracker {
    root: Option<Pid>,
    exited: bool,
    live: bool,
    cleaning: bool,
    next_query: Instant,
}
impl Tracker {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            root: None,
            exited: false,
            live: true,
            cleaning: false,
            next_query: Instant::now(),
        })
    }
    pub(super) fn start(&mut self, target: Pid) -> io::Result<()> {
        self.root = Some(target);
        Ok(())
    }
    pub(super) fn reap(&mut self) -> io::Result<Option<i32>> {
        let Some(root) = self.root.filter(|_| !self.exited) else {
            return Ok(None);
        };
        loop {
            match waitid(
                WaitId::Pid(root),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            ) {
                Ok(Some(status)) => {
                    self.exited = true;
                    self.next_query = Instant::now();
                    return raw(&status).map(Some);
                }
                Ok(None) => return Ok(None),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    pub(super) fn refresh(&mut self) -> io::Result<()> {
        let Some(root) = self.root else {
            self.live = false;
            return Ok(());
        };
        if !self.exited {
            self.live = true;
            return Ok(());
        }
        let now = Instant::now();
        if now >= self.next_query {
            self.next_query = now + QUERY;
            self.live = group_live(root)? || group_live(root)?;
        }
        Ok(())
    }
    pub(super) fn live(&self) -> io::Result<bool> {
        Ok(self.live)
    }
    pub(super) fn timeout(&self) -> Option<Duration> {
        (self.exited && self.live)
            .then(|| self.next_query.saturating_duration_since(Instant::now()))
    }
    pub(super) fn kill(&mut self) -> io::Result<()> {
        if let Some(root) = self.root {
            let _ = kill_process_group(root, Signal::KILL);
            if !self.exited {
                let _ = kill_process(root, Signal::KILL);
            }
            if !self.cleaning {
                self.cleaning = true;
                self.next_query = Instant::now();
            }
        }
        Ok(())
    }
    pub(super) fn release(&mut self) -> io::Result<()> {
        self.reap()?;
        let Some(root) = self.root.take().filter(|_| self.exited) else {
            return Ok(());
        };
        loop {
            match waitpid(Some(root), WaitOptions::empty()) {
                Ok(_) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn group_live(root: Pid) -> io::Result<bool> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .env("LC_ALL", "C")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("cannot enumerate processes"));
    }
    let text = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    let root = root.as_raw_nonzero().get();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let [pid, pgid, stat] = fields[..] else {
            return Err(io::Error::other("invalid ps process row"));
        };
        let pid: i32 = pid.parse().map_err(io::Error::other)?;
        let pgid: i32 = pgid.parse().map_err(io::Error::other)?;
        if pgid == root && pid != root && !stat.starts_with('Z') {
            return Ok(true);
        }
    }
    Ok(false)
}

fn raw(status: &WaitIdStatus) -> io::Result<i32> {
    if let Some(code) = status.exit_status() {
        return Ok(code << 8);
    }
    let signal = status
        .terminating_signal()
        .ok_or_else(|| io::Error::other("invalid target status"))?;
    Ok(signal | if status.dumped() { 0x80 } else { 0 })
}
