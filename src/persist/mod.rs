use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::{self, Read as _, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::Path,
    time::{Duration, Instant},
};

const LOCK_TIMEOUT: Duration = Duration::from_secs(90);
const LOCK_POLL: Duration = Duration::from_millis(50);

pub(crate) struct Lock {
    _file: File,
}

pub(crate) async fn lock(path: &Path) -> io::Result<Lock> {
    let opening = path.to_owned();
    let file = tokio::task::spawn_blocking(move || {
        private_dir(&opening)?;
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(opening.with_extension("lock"))
    })
    .await
    .map_err(io::Error::other)??;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Lock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < LOCK_TIMEOUT => {
                tokio::time::sleep(LOCK_POLL).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::other(format!(
                    "another Bart process holds {}",
                    path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

pub(crate) fn read(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    match open(path) {
        Ok(file) => read_to(&file, limit).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn open(path: &Path) -> io::Result<File> {
    open_options(path, false)
}

pub(crate) fn open_append(path: &Path) -> io::Result<File> {
    open_options(path, true)
}

fn open_options(path: &Path, append: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .append(append)
        .custom_flags(
            (rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOCTTY)
                .bits()
                .cast_signed(),
        )
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

pub(crate) fn prefix(file: &File, limit: u64) -> io::Result<Vec<u8>> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn read_to(file: &File, limit: u64) -> io::Result<Vec<u8>> {
    let bytes = prefix(file, limit.saturating_add(1))?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("larger than {} KiB", limit / 1024),
        ));
    }
    Ok(bytes)
}

pub(crate) fn replace_json(
    path: &Path,
    value: &impl serde::Serialize,
    limit: u64,
) -> Result<(), ReplaceError> {
    let bytes = serde_json::to_vec(value)
        .map_err(io::Error::other)
        .map_err(ReplaceError::Unchanged)?;
    if bytes.len() as u64 > limit {
        return Err(ReplaceError::Unchanged(io::Error::other(format!(
            "{} would exceed its {limit}-byte limit; it is unchanged",
            path.display()
        ))));
    }
    replace(path, &bytes)
}

#[derive(Debug)]
pub(crate) enum ReplaceError {
    Unchanged(io::Error),
    Replaced(io::Error),
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unchanged(error) | Self::Replaced(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unchanged(error) | Self::Replaced(error) => Some(error),
        }
    }
}

pub(crate) fn replace(path: &Path, contents: &[u8]) -> Result<(), ReplaceError> {
    let dir = private_dir(path).map_err(ReplaceError::Unchanged)?;
    let random = crate::random::bytes::<8>().map_err(ReplaceError::Unchanged)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(
        ".{name}.{}-{:016x}.tmp",
        std::process::id(),
        u64::from_ne_bytes(random)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(ReplaceError::Unchanged)?;
    let written = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.map_err(ReplaceError::Unchanged)?;
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(ReplaceError::Replaced)
}

fn private_dir(path: &Path) -> io::Result<&Path> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests;
