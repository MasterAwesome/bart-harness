use std::{
    ffi::{OsStr, OsString},
    os::unix::{ffi::OsStringExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::child::Prepared as Command;

use super::{ENVIRONMENT, Place};
use crate::tool::Network;

pub(super) struct Tools {
    python3: PathBuf,
    systemd_run: PathBuf,
    bwrap: PathBuf,
    git: PathBuf,
}

impl Tools {
    pub(super) fn hidden_directories<'a>(&'a self) -> Vec<&'a Path> {
        let private: Vec<PathBuf> = [
            crate::auth::config_dir().ok(),
            crate::auth::xdg_base("XDG_STATE_HOME", ".local/state")
                .ok()
                .map(|base| base.join("bart")),
        ]
        .into_iter()
        .flatten()
        .collect();
        // A tool in `prefix/bin` needs its prefix (Python's `lib`), unless that would expose Bart's own files.
        let installation = |tool: &'a PathBuf| -> Option<&'a Path> {
            let bin = tool.parent()?;
            Some(
                bin.parent()
                    .filter(|prefix| {
                        bin.file_name() == Some(OsStr::new("bin"))
                            && !private.iter().any(|path| path.starts_with(prefix))
                    })
                    .unwrap_or(bin),
            )
        };
        let mut directories: Vec<&Path> =
            [&self.python3, &self.systemd_run, &self.bwrap, &self.git]
                .into_iter()
                .filter_map(installation)
                .filter(|directory| {
                    [Path::new("/tmp"), Path::new("/run")]
                        .iter()
                        .any(|hidden| directory.starts_with(hidden))
                })
                .collect();
        directories.sort_unstable();
        directories.dedup();
        directories
    }
}

pub(super) fn tools(project: &Path) -> Result<Tools, String> {
    resolve(&std::env::var_os("PATH").unwrap_or_default(), project)
}

fn resolve(path: &OsStr, project: &Path) -> Result<Tools, String> {
    let project = project.canonicalize().map_err(|error| error.to_string())?;
    let find = |name: &str| {
        std::env::split_paths(path)
            .filter(|directory| {
                directory.is_absolute()
                    && directory
                        .canonicalize()
                        .is_ok_and(|directory| !directory.starts_with(&project))
            })
            .filter_map(|directory| directory.join(name).canonicalize().ok())
            .filter(|file| !file.starts_with(&project))
            .find(|file| {
                file.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .ok_or_else(|| {
                format!(
                    "--jail needs {name} in a PATH directory outside {}",
                    project.display()
                )
            })
    };
    Ok(Tools {
        python3: find("python3")?,
        systemd_run: find("systemd-run")?,
        bwrap: find("bwrap")?,
        git: find("git")?,
    })
}

pub(super) fn command(
    program: &str,
    place: &Place,
    env: &std::collections::BTreeMap<String, String>,
    cwd: &Path,
) -> Result<Command, String> {
    let tools = tools(&place.cwd)?;
    let mut command = Command::from_command(tokio::process::Command::from(host_command(
        &tools.systemd_run,
    )))
    .map_err(|error| error.to_string())?;
    let socket = command.socket();
    let executable = &place.executable;
    command.args(RESOURCE_LIMITS);
    let environment = (!env.is_empty())
        .then(|| environment_file(env))
        .transpose()?;
    if let Some(file) = &environment {
        // Open after scope entry; never make a parent descriptor inheritable across concurrent spawns.
        command
            .args([
                "/bin/bash",
                "-c",
                "exec 3<\"$1\" || exit; shift; exec \"$@\"",
                "bart-mcp-env",
            ])
            .arg(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                std::os::fd::AsRawFd::as_raw_fd(file)
            ));
    }
    command
        .arg(&tools.bwrap)
        .args(jail_arguments(place, &tools)?)
        .arg("--chdir")
        .arg(cwd);
    if let Some(file) = environment {
        command.args(["--args", "3"]);
        command.argument_file(file);
    }
    command
        .arg("--ro-bind")
        .arg(executable)
        .arg("/run/bart-supervisor")
        .arg("--ro-bind")
        .arg(socket.parent().expect("socket directory"))
        .arg("/run/bart-control")
        .args([
            "--setenv",
            "BART_SUPERVISOR_SOCKET",
            "/run/bart-control/control",
        ])
        .args([
            "--setenv",
            "BART_SUPERVISOR_ADOPT",
            if program == "bash" { "1" } else { "0" },
        ])
        .args(["/run/bart-supervisor", crate::supervise::MODE])
        .arg(if program == "python3" {
            tools.python3.as_os_str()
        } else {
            OsStr::new(program)
        });
    Ok(command)
}

// `.git`, plus the git directories a gitfile (worktrees, --separate-git-dir) points to inside the project.
fn git_metadata(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let git = cwd.join(".git");
    let Ok(kind) = git.symlink_metadata() else {
        return Ok(Vec::new());
    };
    if kind.is_symlink() {
        return Err(
            "sandbox cannot protect a symlinked .git; replace it with the directory".into(),
        );
    }
    let named = |file: &Path, prefix: &str| {
        use std::{io::Read as _, os::unix::fs::OpenOptionsExt as _};
        let mut text = String::new();
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
            .open(file)
            .ok()
            .filter(|opened| opened.metadata().is_ok_and(|metadata| metadata.is_file()))?
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        let target = text.lines().next()?.strip_prefix(prefix)?.trim();
        Some(file.parent()?.join(target))
    };
    let existing = |path: PathBuf| {
        path.canonicalize().map_err(|_| {
            format!(
                "sandbox cannot protect .git: it names {}, which does not exist",
                path.display()
            )
        })
    };
    let mut found = vec![git.clone()];
    let gitdir = if kind.is_dir() {
        Some(git)
    } else {
        named(&git, "gitdir:").map(existing).transpose()?
    };
    if let Some(gitdir) = gitdir {
        if let Some(common) = named(&gitdir.join("commondir"), "") {
            found.push(existing(common)?);
        }
        found.push(gitdir);
    }
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    found.retain(|path| path.starts_with(cwd) || path.starts_with(&root));
    Ok(found)
}

fn environment_file(
    env: &std::collections::BTreeMap<String, String>,
) -> Result<std::fs::File, String> {
    use std::io::{Seek as _, Write as _};
    let mut bytes = Vec::new();
    for (name, value) in env {
        for part in ["--setenv", name, value] {
            bytes.extend_from_slice(part.as_bytes());
            bytes.push(0);
        }
    }
    let fd = rustix::fs::memfd_create("bart-mcp-env", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|error| error.to_string())?;
    let mut file = std::fs::File::from(fd);
    file.write_all(&bytes)
        .and_then(|()| file.rewind())
        .map_err(|error| error.to_string())?;
    Ok(file)
}

fn host_command(program: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.env_clear().env("PATH", "/usr/bin:/bin");
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

const RESOURCE_LIMITS: [&str; 14] = [
    "--user",
    "--scope",
    "--quiet",
    "--collect",
    "--expand-environment=no",
    "-p",
    "MemoryMax=8G",
    "-p",
    "MemorySwapMax=0",
    "-p",
    "TasksMax=1024",
    "-p",
    "CPUQuota=1200%",
    "--",
];

const TOOLCHAINS: [(&str, &str); 2] = [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")];

pub(in crate::tool::process) fn toolchain_homes(home: &Path) -> [PathBuf; 2] {
    toolchain_homes_from(home, |name| std::env::var_os(name))
}

fn toolchain_homes_from(home: &Path, variable: impl Fn(&str) -> Option<OsString>) -> [PathBuf; 2] {
    TOOLCHAINS.map(|(name, default)| {
        variable(name)
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.join(default), PathBuf::from)
    })
}

fn jail_arguments(place: &Place, tools: &Tools) -> Result<Vec<OsString>, String> {
    let cwd = &place.cwd;
    let home = std::env::home_dir().unwrap_or_else(|| "/nonexistent".into());
    let mut arguments: Vec<OsString> = Vec::new();
    let mut push =
        |parts: &[&std::ffi::OsStr]| arguments.extend(parts.iter().map(|part| part.to_os_string()));
    push(&[
        "--ro-bind".as_ref(),
        "/".as_ref(),
        "/".as_ref(),
        "--dev".as_ref(),
        "/dev".as_ref(),
    ]);
    push(&[
        "--proc".as_ref(),
        "/proc".as_ref(),
        "--tmpfs".as_ref(),
        "/tmp".as_ref(),
        "--tmpfs".as_ref(),
        "/run".as_ref(),
    ]);
    let network = place.confinement.network();
    if network == Network::On
        && let Ok(resolver) = std::fs::canonicalize("/etc/resolv.conf")
        && let Some(directory) = resolver
            .parent()
            .filter(|parent| parent.starts_with("/run"))
    {
        push(&[
            "--ro-bind".as_ref(),
            directory.as_os_str(),
            directory.as_os_str(),
        ]);
    }
    let toolchains = toolchain_homes(&home);
    for path in tools.hidden_directories() {
        push(&["--ro-bind-try".as_ref(), path.as_os_str(), path.as_os_str()]);
    }
    push(&["--bind".as_ref(), cwd.as_os_str(), cwd.as_os_str()]);
    // Git runs repository hooks and config on the host later, so the jail may read them, never write.
    for metadata in git_metadata(cwd)? {
        push(&[
            "--ro-bind".as_ref(),
            metadata.as_os_str(),
            metadata.as_os_str(),
        ]);
    }
    for (path, directory) in protected_paths()? {
        if path.exists() {
            if directory {
                push(&["--tmpfs".as_ref(), path.as_os_str()]);
            } else {
                push(&["--ro-bind".as_ref(), "/dev/null".as_ref(), path.as_os_str()]);
            }
        }
    }
    let pycache = place.pycache_dir();
    push(&[
        "--ro-bind-try".as_ref(),
        place.spill_dir.as_os_str(),
        place.spill_dir.as_os_str(),
        "--bind-try".as_ref(),
        pycache.as_os_str(),
        pycache.as_os_str(),
    ]);
    push(&[
        "--unshare-pid".as_ref(),
        "--die-with-parent".as_ref(),
        "--new-session".as_ref(),
        "--clearenv".as_ref(),
    ]);
    if network == Network::Off {
        push(&["--unshare-net".as_ref()]);
    }
    arguments.extend(jail_environment(&home, &toolchains, tools));
    Ok(arguments)
}

fn jail_environment(home: &Path, toolchains: &[PathBuf], tools: &Tools) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = Vec::new();
    let mut push =
        |parts: &[&std::ffi::OsStr]| arguments.extend(parts.iter().map(|part| part.to_os_string()));
    let path = std::env::var_os("PATH").unwrap_or_default();
    push(&[
        "--setenv".as_ref(),
        "PATH".as_ref(),
        path.as_os_str(),
        "--setenv".as_ref(),
        "HOME".as_ref(),
        home.as_os_str(),
    ]);
    for (name, value) in ENVIRONMENT {
        push(&["--setenv".as_ref(), name.as_ref(), value.as_ref()]);
    }
    for ((variable, _), path) in TOOLCHAINS.iter().zip(toolchains) {
        push(&["--setenv".as_ref(), variable.as_ref(), path.as_os_str()]);
    }
    for (name, value) in git_identity(&tools.git) {
        push(&["--setenv".as_ref(), name.as_ref(), value.as_os_str()]);
    }
    arguments
}

/// The jail hides `~/.gitconfig`, whose other settings may not work inside it.
fn git_identity(git: &Path) -> &'static [(&'static str, OsString)] {
    static IDENTITY: OnceLock<Vec<(&str, OsString)>> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let get = |key| {
            host_command(git)
                .args(["config", "--global", "--get", key])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| OsString::from_vec(output.stdout.trim_ascii_end().to_vec()))
        };
        [
            ("user.name", ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"]),
            ("user.email", ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"]),
        ]
        .into_iter()
        .filter_map(|(key, names)| Some((names, get(key)?)))
        .flat_map(|(names, value)| names.map(|name| (name, value.clone())))
        .collect()
    })
}

pub(super) fn protected_paths() -> Result<Vec<(PathBuf, bool)>, String> {
    let home = std::env::home_dir().ok_or("cannot determine HOME for protected masks")?;
    let mut cargo_roots = vec![home.join(".cargo")];
    if let Some(cargo) = std::env::var_os("CARGO_HOME").filter(|p| !p.is_empty()) {
        cargo_roots.push(PathBuf::from(cargo));
    }
    let state =
        crate::auth::xdg_base("XDG_STATE_HOME", ".local/state").map_err(|e| e.to_string())?;
    let mut paths = vec![
        (crate::auth::config_dir().map_err(|e| e.to_string())?, true),
        (state.join("bart"), true),
    ];
    for cargo in cargo_roots {
        for name in ["credentials", "credentials.toml"] {
            paths.push((cargo.join(name), false));
        }
    }
    let mut protected = Vec::new();
    for (path, directory) in paths {
        let path = std::path::absolute(path).map_err(|e| e.to_string())?;
        let nominal = resolve_missing(path.parent().ok_or("invalid protected root")?)?
            .join(path.file_name().ok_or("invalid protected root")?);
        let resolved = resolve_missing(&path)?;
        protected.push((resolved.clone(), directory));
        if nominal != resolved {
            protected.push((nominal, directory));
        }
    }
    Ok(protected)
}

fn resolve_missing(path: &Path) -> Result<PathBuf, String> {
    if path.symlink_metadata().is_ok() {
        return path
            .canonicalize()
            .map_err(|e| format!("cannot resolve protected path: {e}"));
    }
    let parent = path.parent().ok_or("cannot resolve protected root")?;
    Ok(resolve_missing(parent)?.join(path.file_name().ok_or("invalid protected path")?))
}

#[cfg(test)]
mod tests;
