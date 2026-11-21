use std::{collections::BTreeMap, path::Path};

use serde_json::Value;

use super::{
    Config, Registry, clean,
    config::{self, Server, Settings},
    oauth, path, secrets,
};
use crate::{auth::Store, persist::ReplaceError};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Add(Add),
    AddJson { name: String, json: String },
    List,
    Get { name: String },
    Remove { name: String },
    Login { name: String },
    Logout { name: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Add {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<String>,
    },
    Http {
        name: String,
        url: String,
        header: Vec<String>,
        bearer_token_env: Option<String>,
    },
}

fn from_add(add: Add) -> Result<(String, Settings), String> {
    let (name, transport) = match add {
        Add::Stdio {
            name,
            command,
            args,
            env: pairs,
        } => {
            if command.is_empty() {
                return Err("a local server needs a command".into());
            }
            let mut env = BTreeMap::new();
            for pair in pairs {
                let (key, value) = pair.split_once('=').ok_or("--env takes KEY=VALUE")?;
                if env.insert(key.to_owned(), value.to_owned()).is_some() {
                    return Err(format!("duplicate environment variable: {key}"));
                }
            }
            (name, Server::Stdio { command, args, env })
        }
        Add::Http {
            name,
            url,
            header,
            bearer_token_env,
        } => {
            if url.is_empty() {
                return Err("a remote server needs a URL".into());
            }
            let mut headers = BTreeMap::<String, String>::new();
            for line in header {
                let (key, value) = line.split_once(':').ok_or("--header takes 'Name: value'")?;
                let key = key.trim();
                if headers.keys().any(|name| name.eq_ignore_ascii_case(key)) {
                    return Err(format!("duplicate header: {key}"));
                }
                headers.insert(key.to_owned(), value.trim().to_owned());
            }
            if let Some(variable) = bearer_token_env {
                if headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("Authorization"))
                {
                    return Err("--bearer-token-env conflicts with an Authorization header".into());
                }
                headers.insert("Authorization".into(), format!("Bearer ${{{variable}}}"));
            }
            (name, Server::Http { url, headers })
        }
    };
    Ok((
        name,
        Settings {
            disabled: false,
            transport,
        },
    ))
}

fn saved(file: &Path, result: Result<(), ReplaceError>) -> Result<Option<String>, String> {
    match result {
        Ok(()) => Ok(None),
        Err(ReplaceError::Unchanged(error)) => Err(format!(
            "cannot write {}; MCP configuration remains unchanged: {error}",
            file.display()
        )),
        Err(ReplaceError::Replaced(error)) => Ok(Some(format!(
            "crash durability is uncertain for {}: {error}",
            file.display()
        ))),
    }
}

fn save_config(config: &Config, file: &Path) -> Result<(), String> {
    if let Some(warning) = saved(file, config.save(file))? {
        crate::terminal_text::stderr_line(format_args!(
            "MCP configuration replacement is visible; {warning}"
        ));
    }
    Ok(())
}

pub async fn command(command: Command, config_dir: &Path, store: &Store) -> Result<(), String> {
    let file = path(config_dir);
    let changes = matches!(
        command,
        Command::Add(_) | Command::AddJson { .. } | Command::Remove { .. }
    );
    let _lock = if changes {
        Some(
            crate::persist::lock(&file)
                .await
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let mut config = Config::load(&file)?;
    let entry = |config: &Config, name: &str| -> Result<config::Entry, String> {
        config
            .entries()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, entry)| entry.clone())
            .ok_or_else(|| format!("no MCP server named {name}"))
    };
    match command {
        Command::Add(add) => {
            let (name, entry) = from_add(add)?;
            config.add(&name, config::Entry::Valid(entry))?;
            save_config(&config, &file)?;
            crate::terminal_text::stderr_line(format_args!(
                "Added {name}; `bart mcp get {}` checks it.",
                crate::shell_quote::quote(&name)
            ));
        }
        Command::AddJson { name, json } => {
            let entry: Value =
                serde_json::from_str(&json).map_err(|error| format!("invalid JSON: {error}"))?;
            config.add(&name, config::Entry::new(entry))?;
            save_config(&config, &file)?;
            crate::terminal_text::stderr_line(format_args!("Added {name}."));
        }
        Command::List => {
            let signed_in = store.load().map_err(|error| error.to_string())?;
            for (name, entry) in config.entries() {
                crate::terminal_text::stdout_line(format_args!(
                    "{}",
                    describe(name, entry, signed_in.mcp.contains_key(name))
                ));
            }
        }
        Command::Get { name } => {
            let mut single = Config::default();
            single.add(&name, entry(&config, &name)?)?;
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            let registry = Registry::start(
                &single,
                crate::confinement::Place {
                    executable: std::env::current_exe().map_err(|error| error.to_string())?,
                    spill_dir: cwd.clone(),
                    cwd,
                    confinement: crate::confinement::Confinement::Unrestricted,
                },
                &crate::http_client::new(),
                store,
            );
            let tools = get_tools(&registry, &name).await?;
            crate::terminal_text::stdout_line(format_args!("{name}: ready\n{tools}"));
        }
        Command::Remove { name } => {
            remove_server(&mut config, &file, &name, store).await?;
        }
        Command::Login { name } => {
            let name = name.as_str();
            let entry = entry(&config, name)?;
            let resolved = config::resolve(&entry)?;
            let config::Server::Http { url, .. } = &resolved else {
                return Err(format!(
                    "{name} is a local server; sign-in is for remote ones"
                ));
            };
            oauth::login(name, url, store, |url| {
                crate::auth::open_browser(name, url);
            })
            .await
            .map_err(|error| clean(&error, &secrets(name, Some(&resolved), store)))?;
            crate::terminal_text::stderr_line(format_args!("Signed in to {name}."));
        }
        Command::Logout { name } => {
            if oauth::logout(&name, store).await? {
                crate::terminal_text::stderr_line(format_args!("Signed out of {name}."));
            } else {
                return Err(format!("{name} has no stored sign-in"));
            }
        }
    }
    Ok(())
}

pub(super) async fn get_tools(registry: &Registry, name: &str) -> Result<String, String> {
    let result = registry.tools(Some(name), None).await;
    let closed = registry.close().await;
    match (result, closed) {
        (Ok(tools), Ok(())) => Ok(tools),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

async fn remove_server(
    config: &mut Config,
    file: &Path,
    name: &str,
    store: &Store,
) -> Result<(), String> {
    if !config.remove(name) {
        return Err(format!("no MCP server named {name}"));
    }
    save_config(config, file)
        .map_err(|error| format!("{error}; MCP credentials for {name} were not changed"))?;
    oauth::logout(name, store).await.map_err(|error| {
        format!(
            "{error}; MCP server {name} was removed from configuration; credential removal failed"
        )
    })?;
    crate::terminal_text::stderr_line(format_args!("Removed {name}."));
    Ok(())
}

pub async fn toggle(
    name: &str,
    config_dir: &Path,
    registry: &Registry,
) -> Result<(bool, Option<String>), String> {
    let file = path(config_dir);
    let _lock = crate::persist::lock(&file)
        .await
        .map_err(|error| error.to_string())?;
    let mut config = Config::load(&file)?;
    let enabled = config
        .entries()
        .find(|(candidate, _)| *candidate == name)
        .ok_or_else(|| format!("no MCP server named {name}"))?
        .1
        .disabled();
    let entry = config.set_disabled(name, !enabled)?.clone();
    let warning = saved(&file, config.save(&file))?;
    if let Err(error) = registry.apply(name, &entry).await {
        let choice = if enabled { "enabled" } else { "disabled" };
        let mut message = format!("{choice} in configuration; live change failed: {error}");
        if let Some(warning) = warning {
            message.push_str(&format!("; {warning}"));
        }
        return Err(message);
    }
    Ok((enabled, warning))
}

fn shown_url(url: &str) -> String {
    let Some(mut parsed) = reqwest::Url::parse(url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    else {
        return "(invalid URL)".into();
    };
    parsed.set_fragment(None);
    let credentials = !parsed.username().is_empty() || parsed.password().is_some();
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    if parsed.query().is_some() {
        let names: Vec<String> = parsed
            .query_pairs()
            .map(|(name, _)| name.into_owned())
            .collect();
        parsed
            .query_pairs_mut()
            .clear()
            .extend_pairs(names.iter().map(|name| (name, "…")));
    }
    if credentials {
        format!("{parsed} (credentials in the URL hidden)")
    } else {
        parsed.to_string()
    }
}

fn describe(name: &str, entry: &config::Entry, signed_in: bool) -> String {
    let keys = |field: &str, map: &BTreeMap<String, String>| {
        if map.is_empty() {
            String::new()
        } else {
            format!(
                " ({field}: {})",
                map.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        }
    };
    let target = match entry.settings() {
        Ok(config::Settings {
            transport: config::Server::Stdio { command, args, env },
            ..
        }) => format!("{command} {}{}", args.join(" "), keys("env", env)),
        Ok(config::Settings {
            transport: config::Server::Http { url, headers },
            ..
        }) => format!(
            "{}{}{}",
            shown_url(url),
            keys("headers", headers),
            if signed_in { " (signed in)" } else { "" }
        ),
        Err(error) => format!("unavailable — {error}"),
    };
    let disabled = if entry.disabled() { " (disabled)" } else { "" };
    format!("{name}: {}{disabled}", target.trim_end())
}
