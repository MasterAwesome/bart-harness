use super::{
    app::{CatalogUse, Delivery, Preparation, PreparedInput},
    clipboard,
    composer::Composer,
    picker::{self, Purpose},
};
use crate::{
    agent::{Input, Session, Settings},
    auth::Store,
    history::InlineImage,
    mcp,
    preferences::Current,
    provider::{self, Auth, Provider},
    record::Sessions,
    tui::text::Clean,
};
use reqwest::Client;
use std::{collections::HashMap, path::PathBuf};
use tokio::task::{AbortHandle, JoinSet};

#[derive(Clone)]
pub(super) struct Shared {
    pub(super) loader: CatalogLoader,
    pub(super) opener: crate::agent::Opener,
    pub(super) sessions: Sessions,
    pub(super) state: PathBuf,
    pub(super) directory: Clean,
}

#[derive(Clone)]
pub(super) struct CatalogLoader {
    pub(super) client: Client,
    pub(super) store: Store,
    pub(super) config: PathBuf,
}

impl CatalogLoader {
    pub(super) async fn entries(&self) -> Result<Vec<picker::Entry>, String> {
        let stored = self.store.load().map_err(|error| error.to_string())?;
        let claude_ready = provider::claude_code_available().await;
        Ok(provider::all(&self.config)?
            .into_iter()
            .filter(|provider| provider.auth() != Auth::External || claude_ready)
            .map(|provider| picker::Entry {
                ready: matches!(provider.auth(), Auth::None | Auth::External)
                    || stored.providers.contains_key(provider.name()),
                provider,
            })
            .collect())
    }

    pub(super) async fn load(
        &self,
        provider: &Provider,
    ) -> Result<Vec<crate::provider::Model>, String> {
        let credential = provider::credential(&self.store, &self.client, provider).await?;
        provider::models(&self.client, provider, credential.as_ref())
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) enum Kind {
    Catalog,
    Providers,
    Sessions,
    Open,
    Image,
    Files,
    Mcp,
    Prepare,
    Delete,
    Preferences,
    Remember,
    CopyConversation,
    Close(u64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ticket {
    pub(super) kind: Kind,
    pub(super) seq: u64,
}
pub(super) struct Done {
    pub(super) ticket: Ticket,
    pub(super) result: TaskResult,
}
pub(super) enum TaskResult {
    Remembered(Result<(), String>),
    Closed,
    ConversationCopied(String),
    Failed(String),
    Catalog {
        purpose: CatalogUse,
        name: String,
        result: Result<Vec<crate::provider::Model>, String>,
    },
    Opened(Result<Box<Opened>, String>),
    InputPrepared(Box<Preparation>),
    Resumed(Result<Box<Resumed>, String>),
    Sessions(Purpose, Result<Vec<crate::record::Summary>, String>),
    Providers(Option<picker::Route>, Result<Vec<picker::Entry>, String>),
    ImageRead(Result<Option<InlineImage>, String>),
    FilesIndexed(Result<super::mention::Index, String>),
    McpToggled(String, Result<(bool, Option<String>), String>),
    Deleted(String, Result<(), String>),
}
pub(super) struct Pastes {
    pub(super) artifacts: crate::tool::Artifacts,
    pub(super) next: usize,
    pub(super) previews: super::paste::Previews,
}
pub(super) struct Opened {
    pub(super) session: Session,
    pub(super) id: String,
    pub(super) pastes: Result<Pastes, String>,
    pub(super) transcript: super::transcript::Transcript,
    pub(super) recent_inputs: Vec<String>,
}
pub(super) struct Resumed {
    pub(super) settings: Settings,
    pub(super) record: crate::record::Resumed,
}
pub(super) async fn opened((session, id): (Session, String)) -> Box<Opened> {
    let artifacts = session.tool_context().artifacts();
    let pastes = tokio::task::spawn_blocking(move || {
        let previews =
            super::paste::previews(artifacts.dir()).map_err(|error| error.to_string())?;
        let next = previews
            .keys()
            .max()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("paste numbering exhausted")?;
        Ok(Pastes {
            artifacts,
            next,
            previews,
        })
    })
    .await
    .unwrap_or_else(|error| Err(error.to_string()));
    tokio::task::spawn_blocking(move || {
        let mut transcript = super::transcript::Transcript::with_pastes(
            pastes
                .as_ref()
                .map(|pastes| pastes.previews.clone())
                .unwrap_or_default(),
        );
        transcript.configure_tools(session.live(), session.preferences().effective.ui);
        super::transcript::project(&mut transcript, &session, 120);
        transcript.resumed(session.journal());
        let mut recent_inputs: Vec<_> = session
            .journal()
            .history()
            .iter()
            .rev()
            .filter_map(|message| match message {
                crate::history::Message::User(text) => Some(text.to_string()),
                _ => None,
            })
            .take(64)
            .collect();
        recent_inputs.reverse();
        Box::new(Opened {
            recent_inputs,
            session,
            id,
            pastes,
            transcript,
        })
    })
    .await
    .expect("transcript projection")
}
pub(super) struct Opening {
    pub(super) opener: crate::agent::Opener,
    pub(super) settings: Settings,
    pub(super) preferences: Current,
    pub(super) resumed: Option<crate::record::Resumed>,
}
pub(super) enum Task {
    CopyConversation(Vec<super::transcript::CopySource>),
    Remember(PathBuf, Settings),
    Close {
        ticket: Ticket,
        driver: super::session::Driver,
        sessions: Sessions,
        id: String,
    },
    Catalog {
        loader: CatalogLoader,
        provider: Provider,
        purpose: CatalogUse,
    },
    Providers(CatalogLoader, Option<picker::Route>),
    Sessions(Sessions, Purpose),
    Resume {
        sessions: Sessions,
        id: String,
        config: PathBuf,
    },
    Open(Box<Opening>),
    Image,
    Files(PathBuf),
    Mcp {
        name: String,
        config: PathBuf,
        registry: std::sync::Arc<mcp::Registry>,
    },
    Prepare {
        composer: Composer,
        artifacts: crate::tool::Artifacts,
        delivery: Delivery,
    },
    Delete(Sessions, String),
}
impl Task {
    pub(super) fn kind(&self) -> Kind {
        match self {
            Self::CopyConversation(..) => Kind::CopyConversation,
            Self::Remember(..) => Kind::Remember,
            Self::Close { ticket, .. } => ticket.kind,
            Self::Catalog { .. } => Kind::Catalog,
            Self::Providers(..) => Kind::Providers,
            Self::Sessions(..) | Self::Resume { .. } => Kind::Sessions,
            Self::Open(..) => Kind::Open,
            Self::Image => Kind::Image,
            Self::Files(..) => Kind::Files,
            Self::Mcp { .. } => Kind::Mcp,
            Self::Prepare { .. } => Kind::Prepare,
            Self::Delete(..) => Kind::Delete,
        }
    }
    async fn run(self) -> TaskResult {
        match self {
            Self::CopyConversation(sources) => {
                match tokio::task::spawn_blocking(move || {
                    sources
                        .iter()
                        .map(|source| match source {
                            super::transcript::CopySource::Text(text) => text.to_string(),
                            super::transcript::CopySource::Rows(rows) => rows
                                .iter()
                                .map(super::markdown::RenderRow::source)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .await
                {
                    Ok(text) => TaskResult::ConversationCopied(text),
                    Err(error) => TaskResult::Failed(error.to_string()),
                }
            }
            Self::Remember(state, settings) => {
                TaskResult::Remembered(super::remember(&state, &settings).await)
            }
            Self::Close {
                mut driver,
                sessions,
                id,
                ..
            } => {
                driver.stop().await;
                let _ = tokio::task::spawn_blocking(move || sessions.delete_if_empty(&id)).await;
                TaskResult::Closed
            }
            Self::Catalog {
                loader,
                provider,
                purpose,
            } => TaskResult::Catalog {
                purpose,
                name: provider.name().to_owned(),
                result: loader.load(&provider).await,
            },
            Self::Providers(loader, route) => TaskResult::Providers(route, loader.entries().await),
            Self::Sessions(sessions, purpose) => TaskResult::Sessions(
                purpose,
                tokio::task::spawn_blocking(move || sessions.list_all().map_err(|e| e.to_string()))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
            ),
            Self::Resume {
                sessions,
                id,
                config,
            } => TaskResult::Resumed(
                tokio::task::spawn_blocking(move || {
                    sessions
                        .locate(&id)
                        .and_then(|located| located.resume(&id))
                        .map_err(|e| format!("cannot resume: {e}"))
                        .and_then(|record| {
                            let meta = &record.previous;
                            let settings = Settings {
                                provider: Provider::named(&meta.provider, &config)?,
                                model: meta.model.clone(),
                                reasoning_effort: meta.effort.clone(),
                            };
                            Ok(Box::new(Resumed { settings, record }))
                        })
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
            ),
            Self::Open(opening) => {
                let Opening {
                    opener,
                    settings,
                    preferences,
                    resumed,
                } = *opening;
                TaskResult::Opened(match opener.open(settings, preferences, resumed).await {
                    Ok(next) => Ok(opened(next).await),
                    Err(error) => Err(error),
                })
            }
            Self::Image => TaskResult::ImageRead(clipboard::image().await),
            Self::Files(root) => TaskResult::FilesIndexed(
                tokio::task::spawn_blocking(move || super::mention::index(&root))
                    .await
                    .map_err(|e| format!("cannot index files: {e}")),
            ),
            Self::Mcp {
                name,
                config,
                registry,
            } => {
                let result = mcp::toggle(&name, &config, &registry).await;
                TaskResult::McpToggled(name, result)
            }
            Self::Prepare {
                mut composer,
                artifacts,
                delivery,
            } => {
                match tokio::task::spawn_blocking(move || {
                    let draft = composer.text().to_owned();
                    let result = composer
                        .save_pastes(&artifacts)
                        .map(|()| composer.text_with_references())
                        .map_err(|e| format!("cannot save paste: {e}"))
                        .and_then(|text| {
                            let mut input = Input::from(text);
                            input.resolve_skill()?;
                            Ok(PreparedInput {
                                text: input.text,
                                skills: input.skills,
                            })
                        });
                    let mut previews = composer.paste_previews();
                    if let Ok(prepared) = &result {
                        for (_, reference) in super::paste::references(&prepared.text) {
                            previews.entry(reference.id).or_insert_with(|| {
                                super::paste::load_preview(artifacts.dir(), reference.id)
                            });
                        }
                    }
                    Preparation {
                        previews,
                        draft,
                        delivery,
                        composer,
                        result,
                    }
                })
                .await
                {
                    Ok(preparation) => TaskResult::InputPrepared(Box::new(preparation)),
                    Err(error) => TaskResult::Failed(format!("cannot prepare input: {error}")),
                }
            }
            Self::Delete(sessions, id) => {
                let deleted = id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    sessions
                        .locate(&id)
                        .and_then(|located| located.delete(&id))
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                TaskResult::Deleted(deleted, result)
            }
        }
    }
}
#[derive(Default)]
pub(super) struct Tasks {
    set: JoinSet<Done>,
    owner: HashMap<Kind, (Ticket, AbortHandle)>,
}
impl Tasks {
    pub(super) fn start(&mut self, ticket: Ticket, task: Task) {
        self.cancel(ticket.kind);
        let handle = self.set.spawn(async move {
            Done {
                ticket,
                result: task.run().await,
            }
        });
        self.owner.insert(ticket.kind, (ticket, handle));
    }
    pub(super) fn cancel(&mut self, kind: Kind) {
        if let Some((_, handle)) = self.owner.remove(&kind) {
            handle.abort();
        }
    }
    pub(super) async fn next(&mut self) -> Option<Done> {
        loop {
            match self.set.join_next().await? {
                Ok(done) => {
                    if self
                        .owner
                        .get(&done.ticket.kind)
                        .is_some_and(|(ticket, _)| *ticket == done.ticket)
                    {
                        self.owner.remove(&done.ticket.kind);
                    }
                    return Some(done);
                }
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    if let Some(ticket) = self
                        .owner
                        .values()
                        .find_map(|(ticket, handle)| (handle.id() == error.id()).then_some(*ticket))
                    {
                        self.owner.remove(&ticket.kind);
                        return Some(Done {
                            ticket,
                            result: TaskResult::Failed(format!("UI task failed: {error}")),
                        });
                    }
                }
            }
        }
    }
    pub(super) async fn finish(mut self) {
        for kind in [
            Kind::Catalog,
            Kind::Providers,
            Kind::Sessions,
            Kind::Image,
            Kind::Files,
            Kind::Prepare,
        ] {
            self.cancel(kind);
        }
        while self.set.join_next().await.is_some() {}
    }
}
