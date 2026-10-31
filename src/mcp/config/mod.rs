use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::persist::ReplaceError;

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_SERVERS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum Server {
    Stdio {
        command: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Config {
    servers: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Entry {
    Valid(Settings),
    Invalid { source: Value, error: String },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Settings {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) disabled: bool,
    #[serde(flatten)]
    pub(super) transport: Server,
}

impl Serialize for Entry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Valid(settings) => settings.serialize(serializer),
            Self::Invalid { source, .. } => source.serialize(serializer),
        }
    }
}

impl Entry {
    pub(super) fn new(source: Value) -> Self {
        match parse_entry(&source) {
            Ok(settings) => Self::Valid(settings),
            Err(error) => Self::Invalid { source, error },
        }
    }

    pub(super) fn settings(&self) -> Result<&Settings, &String> {
        match self {
            Self::Valid(settings) => Ok(settings),
            Self::Invalid { error, .. } => Err(error),
        }
    }

    pub(super) fn disabled(&self) -> bool {
        self.settings().is_ok_and(|entry| entry.disabled)
    }
}

pub(crate) fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp.json")
}

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let bytes = match crate::persist::read(path, MAX_CONFIG_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(Self::default()),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
        let Value::Object(mut fields) = value else {
            return Err("configuration root must be an object".into());
        };
        let servers = fields.remove("mcpServers").unwrap_or_else(|| json!({}));
        if !fields.is_empty() {
            return Err("unknown configuration keys".into());
        }
        match servers {
            Value::Object(servers) if servers.len() > MAX_SERVERS => {
                Err(format!("more than {MAX_SERVERS} servers"))
            }
            Value::Object(servers) => {
                if servers.keys().any(|name| {
                    name.is_empty() || name.chars().any(crate::terminal_text::unsafe_character)
                }) {
                    return Err(
                        "server names must be nonempty, printable single-line strings".into(),
                    );
                }
                Ok(Self {
                    servers: servers
                        .into_iter()
                        .map(|(name, source)| (name, Entry::new(source)))
                        .collect(),
                })
            }
            _ => Err("`mcpServers` must be an object of servers".into()),
        }
    }

    pub(super) fn save(&self, path: &Path) -> Result<(), ReplaceError> {
        let fields = json!({"mcpServers": self.servers});
        let text = serde_json::to_string_pretty(&fields)
            .map_err(std::io::Error::other)
            .map_err(ReplaceError::Unchanged)?;
        if self.servers.len() > MAX_SERVERS || text.len() as u64 >= MAX_CONFIG_BYTES {
            return Err(ReplaceError::Unchanged(std::io::Error::other(format!(
                "the configuration would pass its limits of {MAX_SERVERS} servers and {} KiB; it is unchanged",
                MAX_CONFIG_BYTES / 1024
            ))));
        }
        crate::persist::replace(path, format!("{text}\n").as_bytes())
    }

    pub(super) fn add(&mut self, name: &str, entry: Entry) -> Result<(), String> {
        if name.is_empty() || name.chars().any(crate::terminal_text::unsafe_character) {
            return Err(format!("`{name}` is not a usable server name"));
        }
        if self.servers.contains_key(name) {
            return Err(format!("a server named {name} exists; remove it first"));
        }
        entry
            .settings()
            .map_err(|error| format!("{name}: {error}"))?;
        self.servers.insert(name.to_owned(), entry);
        Ok(())
    }

    pub(super) fn remove(&mut self, name: &str) -> bool {
        self.servers.remove(name).is_some()
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.servers
            .iter()
            .map(|(name, entry)| (name.as_str(), entry))
    }

    pub(super) fn set_disabled(&mut self, name: &str, disabled: bool) -> Result<&Entry, String> {
        let entry = self
            .servers
            .get_mut(name)
            .ok_or_else(|| format!("no MCP server named {name}"))?;
        match entry {
            Entry::Valid(settings) => settings.disabled = disabled,
            Entry::Invalid { error, .. } => return Err(error.clone()),
        }
        Ok(entry)
    }
}

pub(super) fn resolve(entry: &Entry) -> Result<Server, String> {
    let expand_map = |map: &BTreeMap<String, String>| -> Result<_, String> {
        map.iter()
            .map(|(key, value)| Ok((key.clone(), expand(value)?)))
            .collect()
    };
    Ok(match &entry.settings()?.transport {
        Server::Stdio { command, args, env } => Server::Stdio {
            command: expand(command)?,
            args: args
                .iter()
                .map(|arg| expand(arg))
                .collect::<Result<_, _>>()?,
            env: expand_map(env)?,
        },
        Server::Http { url, headers } => {
            let url = expand(url)?;
            let parsed =
                reqwest::Url::parse(&url).map_err(|error| format!("invalid MCP URL: {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err("MCP URL must be an absolute HTTP(S) URL with a host".into());
            }
            Server::Http {
                url,
                headers: expand_map(headers)?,
            }
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEntry {
    #[serde(default)]
    disabled: bool,
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    url: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    headers: Option<BTreeMap<String, String>>,
}

fn parse_entry(source: &Value) -> Result<Settings, String> {
    let entry: WireEntry = serde_json::from_value(source.clone())
        .map_err(|_| "invalid server fields: unknown keys or incorrect value types".to_owned())?;
    let transport = match (entry.kind.as_deref(), entry.command, entry.url) {
        (None | Some("stdio"), Some(command), None) if entry.headers.is_none() => Server::Stdio {
            command,
            args: entry.args.unwrap_or_default(),
            env: entry.env.unwrap_or_default(),
        },
        (None | Some("http"), None, Some(url)) if entry.args.is_none() && entry.env.is_none() => {
            Server::Http {
                url,
                headers: entry.headers.unwrap_or_default(),
            }
        }
        _ => {
            return Err(
                "expected a stdio command with args/env or an http URL with headers".into(),
            );
        }
    };
    Ok(Settings {
        disabled: entry.disabled,
        transport,
    })
}

fn expand(text: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .ok_or("unclosed `${` in a configured value")?;
        let reference = &rest[start + 2..start + end];
        let (name, default) = match reference.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (reference, None),
        };
        match (std::env::var(name), default) {
            (Ok(value), _) => out.push_str(&value),
            (Err(_), Some(default)) => out.push_str(default),
            (Err(_), None) => return Err(format!("environment variable {name} is not set")),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests;
