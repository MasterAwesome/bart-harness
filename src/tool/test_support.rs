use std::path::Path;

use super::{Confinement, Context};

pub(crate) fn place(dir: &Path, confinement: crate::tool::Confinement) -> crate::tool::Place {
    crate::tool::Place {
        cwd: dir.to_path_buf(),
        executable: executable(),
        spill_dir: dir.join("spill"),
        confinement,
    }
}

pub(crate) fn executable() -> std::path::PathBuf {
    match option_env!("CARGO_BIN_EXE_bart") {
        Some(path) => path.into(),
        None => panic!("the internal suite runs as a Cargo integration test"),
    }
}

pub(crate) fn context(dir: &Path, confinement: Confinement, output_budget: usize) -> Context {
    Context::new(
        dir.to_path_buf(),
        executable(),
        dir.join("spill"),
        output_budget,
        confinement,
        std::sync::Arc::default(),
    )
    .unwrap()
}

pub(crate) struct BlockedWorker {
    release: std::sync::mpsc::Sender<()>,
    worker: tokio::task::JoinHandle<()>,
}

impl BlockedWorker {
    pub(crate) async fn release(self) {
        self.release.send(()).unwrap();
        self.worker.await.unwrap();
    }
}

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

pub(crate) async fn wait_until_dead(pid: &str) {
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

pub(crate) struct TempDir(tempfile::TempDir);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(&format!("bart-{name}-"))
                .tempdir()
                .unwrap(),
        )
    }

    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
}
