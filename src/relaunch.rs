use crate::{agent::Settings, preferences::Preferences};

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

const SETTINGS_ENV: &str = "BART_SESSION_SETTINGS";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transfer {
    session: String,
    pub(crate) preferences: Preferences,
    pub(crate) choice: crate::choice::Choice,
}

pub(crate) fn settings(id: &str) -> Result<Option<Transfer>, String> {
    let Some(value) = std::env::var_os(SETTINGS_ENV) else {
        return Ok(None);
    };
    let value = value.to_str().ok_or("session settings must be UTF-8")?;
    if value.len() > 8192 {
        return Err("session settings exceed 8 KiB".into());
    }
    let transfer: Transfer = serde_json::from_str(value)
        .map_err(|error| format!("invalid session settings: {error}"))?;
    Ok((transfer.session == id).then_some(transfer))
}

pub(crate) struct Plan {
    command: Command,
}

impl Plan {
    pub(crate) fn new(
        cwd: PathBuf,
        id: &str,
        binary: PathBuf,
        settings: &Settings,
        preferences: &Preferences,
    ) -> Self {
        let mut command = Command::new(binary);
        command.current_dir(cwd).args(["--session", id]);
        command.env(
            SETTINGS_ENV,
            serde_json::to_string(&Transfer {
                session: id.into(),
                preferences: preferences.clone(),
                choice: settings.choice(),
            })
            .expect("settings serialize"),
        );
        Self { command }
    }

    pub(crate) fn execute(mut self) -> std::io::Error {
        self.command.exec()
    }
}

pub(crate) fn command(cwd: &Path, id: &str) -> String {
    use crate::shell_quote::quote;
    format!(
        "cd {} && bart --session {}",
        quote(&cwd.to_string_lossy()),
        quote(id)
    )
}
