use std::{
    collections::BTreeMap,
    fmt, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::persist::{self, Lock};

pub(crate) mod callback;
pub(crate) mod chatgpt;
pub(crate) mod refresh;

const GROUP_OR_OTHER_BITS: u32 = 0o077;
const MAX_STORE_BYTES: u64 = 1024 * 1024;
const MAX_API_KEY_BYTES: usize = 4096;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Secret(String);

impl Secret {
    #[must_use]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Credential {
    ApiKey { key: Secret },
    Chatgpt(chatgpt::Tokens),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SecretValue(pub serde_json::Value);

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    pub(crate) providers: BTreeMap<String, Credential>,
    pub(crate) mcp: BTreeMap<String, SecretValue>,
}

#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    Replace(persist::ReplaceError),
    NotPrivate(PathBuf),
    Malformed {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    InvalidApiKey,
    Login(String),
    Http {
        status: reqwest::StatusCode,
        message: String,
    },
    Transport(reqwest::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "credential store: {error}"),
            Self::Replace(persist::ReplaceError::Unchanged(error)) => write!(
                f,
                "credential store: {error}; the previous credentials are unchanged"
            ),
            Self::Replace(persist::ReplaceError::Replaced(error)) => write!(
                f,
                "credential store: {error}; the new credentials are in place, but their durability is uncertain"
            ),
            Self::NotPrivate(path) => write!(
                f,
                "{} is accessible by group or others; run `chmod 600` on it",
                path.display()
            ),
            Self::Malformed { path, line, column } => {
                write!(
                    f,
                    "credential store {} is malformed at line {line}, column {column}; fix it, or move it aside (it holds every provider and MCP sign-in) and sign in again with `bart login <provider>`",
                    path.display()
                )
            }
            Self::InvalidApiKey => f.write_str("an API key must be one token of printable ASCII"),
            Self::Login(problem) => write!(f, "login failed: {problem}"),
            Self::Http { status, message } => {
                write!(f, "authorization server returned {status}: {message}")
            }
            Self::Transport(error) => write!(f, "cannot reach the authorization server: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Replace(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn api_key(input: &str) -> Result<Secret, Error> {
    let key = input.trim_ascii();
    if key.is_empty() || key.len() > MAX_API_KEY_BYTES || !key.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(Error::InvalidApiKey);
    }
    Ok(Secret(key.to_owned()))
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub(crate) fn config_dir(&self) -> &Path {
        self.path
            .parent()
            .expect("credential store path has a parent")
    }
    #[must_use]
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load(&self) -> Result<Credentials, Error> {
        let file = match crate::persist::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Credentials::default());
            }
            Err(error) => return Err(error.into()),
        };
        // The opened file, not the path, so a swap cannot bypass the check.
        if file.metadata()?.permissions().mode() & GROUP_OR_OTHER_BITS != 0 {
            return Err(Error::NotPrivate(self.path.clone()));
        }
        let text = crate::persist::read_to(&file, MAX_STORE_BYTES)?;
        serde_json::from_slice(&text).map_err(|error| Error::Malformed {
            path: self.path.clone(),
            line: error.line(),
            column: error.column(),
        })
    }

    #[cfg(test)]
    pub(crate) async fn update<R>(
        &self,
        change: impl AsyncFnOnce(&mut Credentials) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let locked = self.lock().await?;
        locked.update(change).await
    }

    pub(crate) async fn lock(&self) -> Result<Locked, Error> {
        Ok(Locked {
            _lock: persist::lock(&self.path).await?,
            store: self.clone(),
        })
    }
}

pub(crate) struct Locked {
    store: Store,
    _lock: Lock,
}

impl Locked {
    pub(crate) async fn update<R>(
        self,
        change: impl AsyncFnOnce(&mut Credentials) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let mut credentials = self.load()?;
        let result = change(&mut credentials).await?;
        self.save(&credentials)?;
        Ok(result)
    }

    pub(crate) fn load(&self) -> Result<Credentials, Error> {
        self.store.load()
    }

    pub(crate) fn save(&self, credentials: &Credentials) -> Result<(), Error> {
        let text = serde_json::to_vec_pretty(credentials).map_err(io::Error::other)?;
        if text.len() as u64 > MAX_STORE_BYTES {
            return Err(Error::Replace(persist::ReplaceError::Unchanged(
                io::Error::other("the credentials would exceed the store's 1 MiB limit"),
            )));
        }
        persist::replace(&self.store.path, &text).map_err(Error::Replace)
    }
}

pub(crate) fn open_browser(what: &str, url: &str) {
    crate::terminal_text::stderr_line(format_args!("Sign in to {what} in your browser:\n{url}\n"));
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command =
        tokio::process::Command::from(crate::supervise::detached(&executable, opener));
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Ok(mut child) = command.spawn() {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

pub(crate) fn config_dir() -> Result<PathBuf, Error> {
    Ok(xdg_base("XDG_CONFIG_HOME", ".config")?.join("bart"))
}

pub(crate) fn xdg_base(variable: &str, fallback: &str) -> io::Result<PathBuf> {
    match std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        Some(dir) => Ok(dir),
        None => std::env::home_dir()
            .map(|home| home.join(fallback))
            .ok_or_else(|| io::Error::other("cannot locate the home directory")),
    }
}

#[must_use]
pub(crate) fn store_path(config_dir: &Path) -> PathBuf {
    config_dir.join("auth.json")
}

#[cfg(test)]
mod tests;
