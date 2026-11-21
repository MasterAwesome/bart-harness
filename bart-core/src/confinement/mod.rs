use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::child::Prepared as Command;

#[cfg(target_os = "linux")]
mod linux;

const ENVIRONMENT: [(&str, &str); 4] = [
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confinement {
    Unrestricted,
    Jailed(Network, Escape),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    Offered,
    Withheld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    On,
    Off,
}

impl Confinement {
    pub fn jailed(self) -> bool {
        matches!(self, Self::Jailed(..))
    }

    pub fn network(self) -> Network {
        match self {
            Self::Unrestricted => Network::On,
            Self::Jailed(network, _) => network,
        }
    }

    pub fn escape(self) -> Escape {
        match self {
            Self::Unrestricted => Escape::Withheld,
            Self::Jailed(_, escape) => escape,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unrestricted => "sandbox off",
            Self::Jailed(Network::On, _) => "sandbox on",
            Self::Jailed(Network::Off, _) => "sandbox on, network off",
        }
    }
}

#[derive(Clone)]
pub struct Place {
    pub cwd: PathBuf,
    pub executable: PathBuf,
    pub spill_dir: PathBuf,
    pub confinement: Confinement,
}

impl Place {
    pub fn pycache_dir(&self) -> PathBuf {
        self.spill_dir.with_file_name("pycache")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Descendants {
    Retain,
    Reap,
}

pub fn check(cwd: &Path, config: &Path, confinement: Confinement) -> Result<(), String> {
    if confinement.jailed() {
        if config
            .canonicalize()
            .is_ok_and(|config| cwd.starts_with(config))
        {
            return Err("sandbox cannot run in Bart's credential directory".into());
        }
        check_jail(cwd)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
use linux::command as jailed_command;

#[cfg(target_os = "linux")]
fn check_jail(project: &Path) -> Result<(), String> {
    let project = project
        .canonicalize()
        .map_err(|error| crate::terminal_text::io_reason(&error))?;
    for (path, _) in linux::protected_paths()? {
        if path.starts_with(&project) || project.starts_with(&path) {
            return Err("sandbox working directory overlaps a protected path: Bart's settings or session records, or cargo credentials".into());
        }
    }
    linux::tools(&project).map(drop)
}

#[cfg(not(target_os = "linux"))]
const UNSUPPORTED: &str = "--jail needs Linux, for bubblewrap and systemd-run";

#[cfg(not(target_os = "linux"))]
fn check_jail(_: &Path) -> Result<(), String> {
    Err(UNSUPPORTED.into())
}

#[cfg(not(target_os = "linux"))]
fn jailed_command(
    _: &str,
    _: &Place,
    _: &BTreeMap<String, String>,
    _: &Path,
    _: Descendants,
) -> Result<Command, String> {
    Err(UNSUPPORTED.into())
}

pub(crate) fn launch(
    program: &str,
    place: &Place,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    descendants: Descendants,
) -> Result<Command, String> {
    match std::fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "cannot enter working directory {}: Not a directory",
                cwd.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot enter working directory {}: {}",
                cwd.display(),
                crate::terminal_text::io_reason(&error)
            ));
        }
    }
    let mut command = match place.confinement {
        Confinement::Unrestricted => {
            let mut command = Command::new(&place.executable, program)
                .map_err(|error| crate::terminal_text::io_reason(&error))?;
            command.envs(ENVIRONMENT).envs(env);
            if descendants == Descendants::Retain {
                command.retain_descendants();
            }
            command
        }
        Confinement::Jailed(..) => jailed_command(program, place, env, cwd, descendants)?,
    };
    command.current_dir(cwd);
    Ok(command)
}
