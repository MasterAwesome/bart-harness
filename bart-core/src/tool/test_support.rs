use std::path::{Path, PathBuf};

use super::{Confinement, Context};

#[cfg(test)]
pub(crate) fn place(dir: &Path, confinement: crate::tool::Confinement) -> crate::tool::Place {
    crate::tool::Place {
        cwd: dir.to_path_buf(),
        executable: executable(),
        spill_dir: dir.join("spill"),
        confinement,
    }
}

#[cfg(test)]
pub(crate) fn executable() -> PathBuf {
    match option_env!("CARGO_BIN_EXE_bart-supervisor") {
        Some(path) => path.into(),
        None => panic!("the internal suite runs as a Cargo integration test"),
    }
}

pub fn context_with(
    dir: &Path,
    executable: PathBuf,
    confinement: Confinement,
    output_budget: usize,
) -> Context {
    Context::new(
        dir.to_path_buf(),
        executable,
        dir.join("spill"),
        output_budget,
        confinement,
        std::sync::Arc::default(),
    )
    .unwrap()
}

#[cfg(test)]
pub(crate) fn context(dir: &Path, confinement: Confinement, output_budget: usize) -> Context {
    context_with(dir, executable(), confinement, output_budget)
}

#[cfg(test)]
pub(crate) struct BlockedWorker {
    release: std::sync::mpsc::Sender<()>,
    worker: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
impl BlockedWorker {
    pub(crate) async fn release(self) {
        self.release.send(()).unwrap();
        self.worker.await.unwrap();
    }
}

#[cfg(test)]
pub(crate) async fn blocked_worker() -> BlockedWorker {
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let _ = started.send(());
        let _ = blocked.recv();
    });
    ready.await.unwrap();
    BlockedWorker { release, worker }
}

pub async fn wait_until_dead(pid: &str) {
    assert!(
        pid.parse::<u32>().is_ok_and(|pid| pid > 0),
        "invalid process ID: {pid:?}"
    );
    for _ in 0..100 {
        let state = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&state.stdout);
        if state.trim().is_empty() || state.trim_start().starts_with('Z') {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("process {pid} is still running");
}

pub struct TempDir(tempfile::TempDir);

impl TempDir {
    pub fn new(name: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(&format!("bart-{name}-"))
                .tempdir()
                .unwrap(),
        )
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}
