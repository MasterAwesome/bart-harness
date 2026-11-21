use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use serde_json::value::{RawValue, to_raw_value};

use super::{Custom, Provider};
use crate::persist;

const MAX_PROVIDERS_BYTES: u64 = 64 * 1024;

impl Provider {
    pub fn named(name: &str, config_dir: &Path) -> Result<Self, String> {
        match name {
            "chatgpt" => return Ok(Self::builtin()),
            "claude" => return Ok(Self::claude()),
            _ => {}
        }
        let raw = load_custom(config_dir)
            .map_err(|error| configuration(&error))?
            .remove(name)
            .ok_or_else(|| format!("no provider named {name}"))?;
        let custom: Custom = serde_json::from_str(raw.get())
            .map_err(|error| format!("{}: {error}", providers_path(config_dir).display()))?;
        Self::custom(name, &custom)
    }
}

pub fn all(config_dir: &Path) -> Result<Vec<Provider>, String> {
    let custom = load_custom(config_dir).map_err(|error| configuration(&error))?;
    [Provider::builtin(), Provider::claude()]
        .into_iter()
        .map(Ok)
        .chain(custom.into_iter().map(|(name, raw)| {
            let custom: Custom = serde_json::from_str(raw.get())
                .map_err(|error| format!("{}: {error}", providers_path(config_dir).display()))?;
            Provider::custom(&name, &custom)
                .map_err(|error| format!("{}: {error}", providers_path(config_dir).display()))
        }))
        .collect()
}

fn configuration(error: &io::Error) -> String {
    format!("cannot read the provider configuration: {error}")
}

#[must_use]
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(name, "chatgpt" | "claude")
}

fn providers_path(config_dir: &Path) -> PathBuf {
    config_dir.join("providers.json")
}

fn load_custom(config_dir: &Path) -> io::Result<BTreeMap<String, Box<RawValue>>> {
    match persist::read(&providers_path(config_dir), MAX_PROVIDERS_BYTES)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        None => Ok(BTreeMap::new()),
    }
}

pub struct CustomUpdate {
    path: PathBuf,
    _lock: persist::Lock,
    after: Vec<u8>,
    changed: bool,
}

impl CustomUpdate {
    pub async fn prepare(
        config_dir: &Path,
        name: &str,
        custom: Option<Custom>,
    ) -> io::Result<Self> {
        let path = providers_path(config_dir);
        let lock = persist::lock(&path).await?;
        let before = persist::read(&path, MAX_PROVIDERS_BYTES)?;
        let mut providers: BTreeMap<String, Box<RawValue>> = before
            .as_ref()
            .map(|bytes| serde_json::from_slice(bytes))
            .transpose()?
            .unwrap_or_default();
        let changed = match custom {
            Some(custom) => {
                Provider::custom(name, &custom).map_err(io::Error::other)?;
                let value = to_raw_value(&custom).map_err(io::Error::other)?;
                providers.insert(name.into(), value);
                true
            }
            None => providers.remove(name).is_some(),
        };
        let after = serde_json::to_vec_pretty(&providers)?;
        if after.len() as u64 > MAX_PROVIDERS_BYTES {
            return Err(io::Error::other(
                "the provider configuration would pass 64 KiB; it is unchanged",
            ));
        }
        Ok(Self {
            path,
            _lock: lock,
            after,
            changed,
        })
    }

    pub fn save(&self) -> Result<bool, String> {
        if self.changed {
            persist::replace(&self.path, &self.after).map_err(|error| {
                let state = match &error {
                    persist::ReplaceError::Unchanged(_) => "the previous provider configuration remains unchanged",
                    persist::ReplaceError::Replaced(_) => "the provider configuration replacement is visible, but its durability is uncertain",
                };
                format!("cannot write provider configuration: {error}; {state}")
            })?;
        }
        Ok(self.changed)
    }
}

pub async fn commit_identity(
    store: &crate::auth::Store,
    name: &str,
    credential: Option<crate::auth::Credential>,
    update: Option<&CustomUpdate>,
) -> Result<(bool, bool), String> {
    let locked = store.lock().await.map_err(|error| error.to_string())?;
    let mut after = locked.load().map_err(|error| error.to_string())?;
    let had_credential = after.providers.contains_key(name);
    let state = if let Some(credential) = credential {
        after.providers.insert(name.into(), credential);
        "credentials were saved"
    } else {
        after.providers.remove(name);
        if had_credential {
            "credentials were removed"
        } else {
            "no credentials were stored"
        }
    };
    locked
        .save(&after)
        .map_err(|error| format!("{error}; provider configuration was not changed"))?;
    let had_provider = update
        .map(CustomUpdate::save)
        .transpose()
        .map_err(|error| format!("{error}; {state} for {name}"))?
        .unwrap_or(false);
    Ok((had_credential, had_provider))
}

pub async fn logout(config: &Path, store: &crate::auth::Store, name: &str) -> Result<(), String> {
    let update = CustomUpdate::prepare(config, name, None)
        .await
        .map_err(|error| {
            format!("cannot prepare provider removal: {error}; credentials were not changed")
        })?;
    let (had_credential, had_provider) = commit_identity(store, name, None, Some(&update)).await?;
    let done = match (had_credential, had_provider) {
        (false, false) => return Err(format!("nothing is stored for {name}")),
        (true, true) => "provider and its key",
        (true, false) => "credentials",
        (false, true) => "provider",
    };
    crate::terminal_text::stderr_line(format_args!("Removed the {done} for {name}."));
    Ok(())
}
