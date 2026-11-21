use std::{collections::BTreeMap, io, path::Path};

use serde::{Deserialize, Serialize};

const MAX_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ui {
    #[default]
    Cells,
    ToolsOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    #[default]
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Fast,
    Ultrafast,
}

impl ServiceTier {
    pub const ALL: [Self; 7] = [
        Self::Auto,
        Self::Default,
        Self::Flex,
        Self::Scale,
        Self::Priority,
        Self::Fast,
        Self::Ultrafast,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Default => "default",
            Self::Flex => "flex",
            Self::Scale => "scale",
            Self::Priority => "priority",
            Self::Fast => "fast",
            Self::Ultrafast => "ultrafast",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, BTreeMap<String, ModelPreferences>>,
    pub permissions: Permissions,
    #[serde(skip)]
    pub service_tier: ServiceTier,
    pub wait_for_reset: bool,
    pub reasoning_summaries: bool,
    pub diffs: bool,
    pub ui: Ui,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<crate::choice::Choice>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    pub approval: crate::approval::Approval,
    pub sandbox: Sandbox,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sandbox {
    pub enabled: bool,
    pub network: crate::confinement::Network,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            enabled: false,
            network: crate::confinement::Network::On,
        }
    }
}

impl Permissions {
    pub fn confinement(&self) -> Result<crate::confinement::Confinement, String> {
        if !self.sandbox.enabled {
            return Ok(crate::confinement::Confinement::Unrestricted);
        }
        if !cfg!(target_os = "linux") {
            return Err("sandbox needs Linux".into());
        }
        let escape = match self.approval.mode() {
            crate::approval::Mode::None => crate::confinement::Escape::Withheld,
            crate::approval::Mode::Manual | crate::approval::Mode::Auto => {
                crate::confinement::Escape::Offered
            }
        };
        Ok(crate::confinement::Confinement::Jailed(
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
            service_tier: ServiceTier::Auto,
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
pub struct ContextWindow(u64);

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
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPreferences {
    context_window: ContextWindow,
}

impl Preferences {
    pub fn context_override(&self, provider: &str, model: &str) -> Option<ContextWindow> {
        Some(self.models.get(provider)?.get(model)?.context_window)
    }

    pub fn window(
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

    pub fn load(config: &Path) -> io::Result<Self> {
        let path = config.join("settings.json");
        match crate::persist::read(&path, MAX_BYTES)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display()))),
            None => Ok(Self::default()),
        }
    }

    pub fn save(&self, config: &Path) -> Result<(), crate::persist::ReplaceError> {
        crate::persist::replace_json(&config.join("settings.json"), self, MAX_BYTES)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Current {
    pub saved: Preferences,
    pub effective: Preferences,
}

impl Current {
    pub fn override_permissions(
        &mut self,
        approval: Option<crate::approval::Mode>,
        jail: Option<crate::confinement::Network>,
    ) -> Result<(), &'static str> {
        if let Some(mode) = approval {
            self.effective.permissions.approval.set_mode(mode)?;
        }
        if let Some(network) = jail {
            self.effective.permissions.sandbox.enabled = true;
            if network == crate::confinement::Network::Off {
                self.effective.permissions.sandbox.network = network;
            }
        }
        Ok(())
    }

    pub fn load(config: &Path) -> io::Result<Self> {
        let saved = Preferences::load(config)?;
        Ok(Self {
            effective: saved.clone(),
            saved,
        })
    }

    pub fn apply(&mut self, change: &Change) -> Result<(), &'static str> {
        if !matches!(change, Change::ServiceTier(_)) {
            change.apply(&mut self.saved)?;
        }
        change.apply(&mut self.effective)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    ContextWindow {
        provider: String,
        model: String,
        tokens: Option<ContextWindow>,
    },
    Approval(crate::approval::Mode),
    Reviewer(crate::choice::Choice),
    Sandbox(bool),
    Network(crate::confinement::Network),
    ServiceTier(ServiceTier),
    WaitForReset(bool),
    ReasoningSummaries(bool),
    Diffs(bool),
    Ui(Ui),
    Search(Option<crate::choice::Choice>),
}

impl Change {
    pub fn apply(&self, preferences: &mut Preferences) -> Result<(), &'static str> {
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
            Self::ServiceTier(tier) => preferences.service_tier = *tier,
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
