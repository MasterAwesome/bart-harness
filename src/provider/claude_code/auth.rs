use std::process::Stdio;

use serde::Deserialize;
use tokio::{
    io::AsyncReadExt,
    process::Child,
    time::{Duration, timeout},
};

use super::wire::failure;
use crate::provider::Error;

pub(super) fn executable() -> std::ffi::OsString {
    std::env::var_os("BART_CLAUDE_PATH").unwrap_or_else(|| "claude".into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    logged_in: bool,
}

struct Probe(Child);

impl Drop for Probe {
    fn drop(&mut self) {
        super::wire::kill_group(&self.0);
    }
}

pub(super) async fn authenticated() -> Result<(), Error> {
    let mut command = tokio::process::Command::from(
        crate::supervise::detached(executable()).map_err(|error| failure(error.to_string()))?,
    );
    let mut child = Probe(
        command
            .args([
                "--setting-sources=user",
                "--settings",
                "{\"disableAllHooks\":true}",
                "auth",
                "status",
                "--json",
            ])
            .env("DISABLE_AUTOUPDATER", "1")
            .env("DISABLE_UPDATES", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| {
                failure("Claude Code is not available; install it and configure its authentication")
            })?,
    );
    let check = async {
        let mut bytes = Vec::new();
        child
            .0
            .stdout
            .take()
            .expect("piped stdout")
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| failure("cannot read Claude Code authentication status"))?;
        if bytes.len() > 64 * 1024 {
            return Err(failure("Claude Code authentication status exceeded 64 KiB"));
        }
        let exit = child
            .0
            .wait()
            .await
            .map_err(|_| failure("cannot check Claude Code authentication status"))?;
        let status: Status = serde_json::from_slice(&bytes)
            .map_err(|_| failure("Claude Code did not return a valid authentication status"))?;
        if !exit.success() || !status.logged_in {
            return Err(failure(
                "Claude Code is not authenticated; configure authentication in Claude Code",
            ));
        }
        Ok(())
    };
    timeout(Duration::from_secs(5), check)
        .await
        .map_err(|_| failure("Claude Code authentication status timed out"))?
}
