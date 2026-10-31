use std::{
    path::Path,
    time::{Duration, Instant},
};

use reqwest::Client;

use super::Settings;
use crate::{
    auth::Store,
    preferences::{Change, Current},
    provider::{self, Model, Request},
    tool::{self, Confinement, WebRoute},
};

const CATALOG_RETRY_INTERVAL: Duration = Duration::from_secs(30);

pub(super) struct Routing {
    pub(super) client: Client,
    pub(super) transport: crate::provider::Transport,
    pub(super) store: Store,
    settings: Settings,
    preferences: Current,
    catalog: Catalog,
}

enum Catalog {
    NotLooked,
    Failed { retry_at: Instant, reason: String },
    Looked(Option<Model>),
}

pub(super) struct Preferred {
    pub(super) confinement: Option<(Confinement, crate::mcp::Config)>,
    pub(super) warning: Option<String>,
}

impl Routing {
    pub(super) fn new(
        client: Client,
        store: Store,
        settings: Settings,
        preferences: Current,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self {
            client,
            transport: crate::provider::Transport::new(cwd),
            store,
            settings,
            preferences,
            catalog: Catalog::NotLooked,
        }
    }

    pub(super) fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(super) fn preferences(&self) -> &Current {
        &self.preferences
    }

    pub(super) fn select(&mut self, settings: Settings) {
        self.settings = settings;
        self.catalog = Catalog::NotLooked;
    }

    pub(super) async fn entry(&mut self) -> Option<&Model> {
        if matches!(self.catalog, Catalog::NotLooked)
            || matches!(self.catalog, Catalog::Failed { retry_at, .. } if Instant::now() >= retry_at)
        {
            let provider = &self.settings.provider;
            let lookup = async {
                let credential = provider::credential(&self.store, &self.client, provider).await?;
                provider::model(
                    &self.client,
                    provider,
                    credential.as_ref(),
                    &self.settings.model,
                )
                .await
                .map_err(|error| error.to_string())
            }
            .await;
            self.catalog = match lookup {
                Ok(model) => Catalog::Looked(model),
                Err(reason) => Catalog::Failed {
                    reason,
                    retry_at: Instant::now() + CATALOG_RETRY_INTERVAL,
                },
            };
        }
        self.cached()
    }

    pub(super) fn cached(&self) -> Option<&Model> {
        match &self.catalog {
            Catalog::Looked(entry) => entry.as_ref(),
            Catalog::NotLooked | Catalog::Failed { .. } => None,
        }
    }

    pub(super) fn sees_images(&self) -> bool {
        !matches!(&self.catalog, Catalog::Looked(Some(model)) if model.images == Some(false))
    }

    pub(super) fn compaction_threshold(&self) -> Result<u64, super::Error> {
        Ok(super::compact::threshold(self.window()?))
    }

    pub(super) fn window(&self) -> Result<u64, super::Error> {
        let entry = self.cached();
        let preferences = &self.preferences.effective;
        let provider = self.settings.provider.name();
        let model = &self.settings.model;
        preferences
            .window(provider, model, entry)
            .map_err(super::Error::Context)?
            .filter(|&tokens| tokens > 0)
            .ok_or_else(|| {
                let reason = match &self.catalog {
                    Catalog::Failed { reason, .. } => format!("its model list could not be loaded: {reason}"),
                    _ => "its model list does not report it".into(),
                };
                super::Error::UnknownWindow(format!(
                    "the context window of {model} on {provider} is unknown ({reason}); set it with /settings, or as models.{provider}.{model}.context_window in {}",
                    self.store.config_dir().join("settings.json").display()
                ))
            })
    }

    pub(super) fn web_route(
        &self,
        diagnostics: Option<std::sync::Arc<crate::record::diagnostics::Log>>,
    ) -> Option<WebRoute> {
        let choice = self.preferences.effective.search.as_ref()?;
        let provider = provider::Provider::named(&choice.provider, self.store.config_dir()).ok()?;
        provider.capabilities().web_search.then(|| WebRoute {
            client: self.client.clone(),
            store: self.store.clone(),
            provider,
            model: choice.model.clone(),
            effort: choice.effort.clone(),
            diagnostics,
        })
    }

    pub(super) fn request<'a>(
        &'a self,
        history: &'a [crate::history::Message],
        tools: &'a [tool::Definition],
        instructions: &'a str,
        cache: Option<crate::provider::Cache<'a>>,
    ) -> Request<'a> {
        let entry = self.cached();
        Request {
            provider: &self.settings.provider,
            model: &self.settings.model,
            instructions,
            history,
            tools: provider::Tools::Functions(tools),
            reasoning_effort: self
                .settings
                .reasoning_effort
                .as_deref()
                .or_else(|| entry.and_then(|model| model.default_effort.as_deref())),
            summaries: self.preferences.effective.reasoning_summaries
                && entry.and_then(|model| model.summaries).unwrap_or(true),
            verbosity: entry.and_then(|model| model.verbosity.as_deref()),
            max_output_tokens: entry.and_then(|model| model.max_output_tokens),
            cache,
        }
    }

    pub(super) async fn prefer(
        &mut self,
        change: &Change,
        cwd: &Path,
        confinement: Confinement,
    ) -> Result<Preferred, String> {
        if let Change::ContextWindow {
            provider, model, ..
        } = change
        {
            if provider != self.settings.provider.name() || model != &self.settings.model {
                return Err("context window edit belongs to another model".into());
            }
            let entry = self.entry().await.cloned();
            let mut proposed = self.preferences.effective.clone();
            change.apply(&mut proposed)?;
            proposed.window(provider, model, entry.as_ref())?;
        }
        let config = self.store.config_dir().to_owned();
        let lock = crate::persist::lock(&config.join("settings.json"))
            .await
            .map_err(|error| format!("cannot lock settings: {error}"))?;
        let mut preferences = self.preferences.clone();
        let change = change.clone();
        let cwd = cwd.to_owned();
        let (preferences, preferred) = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            preferences.saved = crate::preferences::Preferences::load(&config)
                .map_err(|error| format!("cannot read settings: {error}"))?;
            preferences.apply(&change)?;
            let next = preferences.effective.permissions.confinement()?;
            let confinement = if next == confinement {
                None
            } else {
                tool::check_confinement(&cwd, &config, next)?;
                Some((next, crate::mcp::Config::load(&crate::mcp::path(&config))?))
            };
            let warning = match preferences.saved.save(&config) {
                Ok(()) => None,
                Err(crate::persist::ReplaceError::Replaced(error)) => Some(format!(
                    "settings applied, but crash durability is uncertain: {error}"
                )),
                Err(crate::persist::ReplaceError::Unchanged(error)) => {
                    return Err(format!("cannot save settings: {error}"));
                }
            };
            Ok::<_, String>((
                preferences,
                Preferred {
                    confinement,
                    warning,
                },
            ))
        })
        .await
        .map_err(|error| format!("settings worker failed: {error}"))??;
        self.preferences = preferences;
        Ok(preferred)
    }
}

#[cfg(test)]
impl Routing {
    pub(super) fn assume(&mut self, entry: Option<Model>) {
        self.catalog = Catalog::Looked(entry);
    }

    pub(super) fn preferences_mut(&mut self) -> &mut Current {
        &mut self.preferences
    }
}
