use std::{collections::BTreeMap, io, path::Path};

use serde::{Deserialize, Serialize};

const MAX_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Ui {
    #[default]
    Cells,
    ToolsOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Preferences {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) models: BTreeMap<String, BTreeMap<String, ModelPreferences>>,
    pub(crate) permissions: Permissions,
    pub(crate) wait_for_reset: bool,
    pub(crate) reasoning_summaries: bool,
    pub(crate) diffs: bool,
    pub(crate) ui: Ui,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) search: Option<crate::choice::Choice>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Permissions {
    pub(crate) approval: crate::approval::Approval,
    pub(crate) sandbox: Sandbox,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Sandbox {
    pub(crate) enabled: bool,
    pub(crate) network: crate::tool::Network,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            enabled: false,
            network: crate::tool::Network::On,
        }
    }
}

impl Permissions {
    pub(crate) fn confinement(&self) -> Result<crate::tool::Confinement, String> {
        if !self.sandbox.enabled {
            return Ok(crate::tool::Confinement::Unrestricted);
        }
        if !cfg!(target_os = "linux") {
            return Err("sandbox needs Linux".into());
        }
        let escape = match self.approval.mode() {
            crate::approval::Mode::None => crate::tool::Escape::Withheld,
            crate::approval::Mode::Manual | crate::approval::Mode::Auto => {
                crate::tool::Escape::Offered
            }
        };
        Ok(crate::tool::Confinement::Jailed(
            self.sandbox.network,
            escape,
        ))
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            models: BTreeMap::new(),
            permissions: Permissions::default(),
            wait_for_reset: true,
            reasoning_summaries: true,
            diffs: true,
            ui: Ui::Cells,
            search: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u64")]
pub(crate) struct ContextWindow(u64);

impl TryFrom<u64> for ContextWindow {
    type Error = String;

    fn try_from(value: u64) -> Result<Self, String> {
        const MINIMUM: u64 = 10_000;
        if value < MINIMUM {
            return Err(format!(
                "context window must be at least the minimum of {MINIMUM} tokens"
            ));
        }
        Ok(Self(value))
    }
}

impl ContextWindow {
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelPreferences {
    context_window: ContextWindow,
}

impl Preferences {
    pub(crate) fn context_override(&self, provider: &str, model: &str) -> Option<ContextWindow> {
        Some(self.models.get(provider)?.get(model)?.context_window)
    }

    pub(crate) fn window(
        &self,
        provider: &str,
        model: &str,
        catalog: Option<&crate::provider::Model>,
    ) -> Result<Option<u64>, String> {
        let value = self.context_override(provider, model);
        if let (Some(value), Some(maximum)) = (value, catalog.and_then(|m| m.max_context_window))
            && value.get() > maximum
        {
            return Err(format!(
                "context window exceeds the maximum of {maximum} tokens"
            ));
        }
        Ok(value
            .map(ContextWindow::get)
            .or_else(|| catalog.and_then(|m| m.context_window)))
    }

    pub(crate) fn load(config: &Path) -> io::Result<Self> {
        let path = config.join("settings.json");
        match crate::persist::read(&path, MAX_BYTES)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display()))),
            None => Ok(Self::default()),
        }
    }

    pub(crate) fn save(&self, config: &Path) -> Result<(), crate::persist::ReplaceError> {
        crate::persist::replace_json(&config.join("settings.json"), self, MAX_BYTES)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Current {
    pub(crate) saved: Preferences,
    pub(crate) effective: Preferences,
}

impl Current {
    pub(crate) fn override_permissions(
        &mut self,
        approval: Option<crate::approval::Mode>,
        jail: Option<crate::tool::Network>,
    ) -> Result<(), &'static str> {
        if let Some(mode) = approval {
            self.effective.permissions.approval.set_mode(mode)?;
        }
        if let Some(network) = jail {
            self.effective.permissions.sandbox.enabled = true;
            if network == crate::tool::Network::Off {
                self.effective.permissions.sandbox.network = network;
            }
        }
        Ok(())
    }

    pub(crate) fn load(config: &Path) -> io::Result<Self> {
        let saved = Preferences::load(config)?;
        Ok(Self {
            effective: saved.clone(),
            saved,
        })
    }

    pub(crate) fn apply(&mut self, change: &Change) -> Result<(), &'static str> {
        change.apply(&mut self.saved)?;
        change.apply(&mut self.effective)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    ContextWindow {
        provider: String,
        model: String,
        tokens: Option<ContextWindow>,
    },
    Approval(crate::approval::Mode),
    Reviewer(crate::choice::Choice),
    Sandbox(bool),
    Network(crate::tool::Network),
    WaitForReset(bool),
    ReasoningSummaries(bool),
    Diffs(bool),
    Ui(Ui),
    Search(Option<crate::choice::Choice>),
}

impl Change {
    pub(crate) fn apply(&self, preferences: &mut Preferences) -> Result<(), &'static str> {
        match self {
            Self::ContextWindow {
                provider,
                model,
                tokens,
            } => {
                let models = preferences.models.entry(provider.clone()).or_default();
                match tokens {
                    Some(context_window) => {
                        models.insert(
                            model.clone(),
                            ModelPreferences {
                                context_window: *context_window,
                            },
                        );
                    }
                    None => {
                        models.remove(model);
                    }
                }
                if models.is_empty() {
                    preferences.models.remove(provider);
                }
            }
            Self::Approval(mode) => return preferences.permissions.approval.set_mode(*mode),
            Self::Reviewer(reviewer) => preferences.permissions.approval.auto(reviewer.clone()),
            Self::Sandbox(enabled) => preferences.permissions.sandbox.enabled = *enabled,
            Self::Network(network) => preferences.permissions.sandbox.network = *network,
            Self::WaitForReset(enabled) => preferences.wait_for_reset = *enabled,
            Self::ReasoningSummaries(enabled) => preferences.reasoning_summaries = *enabled,
            Self::Diffs(enabled) => preferences.diffs = *enabled,
            Self::Ui(ui) => preferences.ui = *ui,
            Self::Search(route) => preferences.search.clone_from(route),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
