use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use super::{Custom, Provider};
use crate::persist;

const MAX_PROVIDERS_BYTES: u64 = 64 * 1024;

impl Provider {
    pub(crate) fn named(name: &str, config_dir: &Path) -> Result<Self, String> {
        match name {
            "chatgpt" => return Ok(Self::builtin()),
            "claude" => return Ok(Self::claude()),
            _ => {}
        }
        let custom = load_custom(config_dir)
            .map_err(|error| configuration(&error))?
            .remove(name)
            .ok_or_else(|| format!("no provider named {name}"))?;
        Self::custom(name, &custom)
    }
}

pub(crate) fn all(config_dir: &Path) -> Result<Vec<Provider>, String> {
    let custom = load_custom(config_dir).map_err(|error| configuration(&error))?;
    [Provider::builtin(), Provider::claude()]
        .into_iter()
        .map(Ok)
        .chain(custom.iter().map(|(name, custom)| {
            Provider::custom(name, custom)
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

fn load_custom(config_dir: &Path) -> io::Result<BTreeMap<String, Custom>> {
    match persist::read(&providers_path(config_dir), MAX_PROVIDERS_BYTES)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        None => Ok(BTreeMap::new()),
    }
}

pub(crate) struct CustomUpdate {
    path: PathBuf,
    _lock: persist::Lock,
    after: Vec<u8>,
    changed: bool,
}

impl CustomUpdate {
    pub(crate) async fn prepare(
        config_dir: &Path,
        name: &str,
        custom: Option<Custom>,
    ) -> io::Result<Self> {
        let path = providers_path(config_dir);
        let lock = persist::lock(&path).await?;
        let before = persist::read(&path, MAX_PROVIDERS_BYTES)?;
        let mut providers: BTreeMap<String, Custom> = before
            .as_ref()
            .map(|bytes| serde_json::from_slice(bytes))
            .transpose()?
            .unwrap_or_default();
        let changed = match custom {
            Some(custom) => {
                Provider::custom(name, &custom).map_err(io::Error::other)?;
                providers.insert(name.into(), custom);
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

    pub(crate) fn save(&self) -> Result<bool, String> {
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
