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
        provider::models(&self.client, &self.store, provider)
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

impl Kind {
    fn cancel_on_quit(self) -> bool {
        match self {
            Self::Catalog
            | Self::Providers
            | Self::Sessions
            | Self::Image
            | Self::Files
            | Self::Prepare
            | Self::CopyConversation => true,
            Self::Open
            | Self::Mcp
            | Self::Delete
            | Self::Preferences
            | Self::Remember
            | Self::Close(_) => false,
        }
    }
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
    pub(super) delete_previous: Option<String>,
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
            delete_previous: None,
            session,
            id,
            pastes,
            transcript,
        })
    })
    .await
    .expect("transcript projection")
}
pub(super) enum OpenFrom {
    New,
    Resume(Box<crate::record::Resumed>),
    DeleteCurrent(String),
}

pub(super) struct Opening {
    pub(super) opener: crate::agent::Opener,
    pub(super) settings: Settings,
    pub(super) preferences: Current,
    pub(super) from: OpenFrom,
}
pub(super) enum Task {
    CopyConversation(Vec<super::transcript::CopySource>),
    Remember(PathBuf, Settings),
    Close {
        ticket: Ticket,
        driver: super::session::Driver,
        delete: bool,
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

pub(super) async fn delete_if_empty(sessions: Sessions, id: &str) -> Result<bool, String> {
    let owned = id.to_owned();
    tokio::task::spawn_blocking(move || sessions.delete_if_empty(&owned))
        .await
        .map_err(std::io::Error::other)
        .and_then(|result| result)
        .map_err(|error| format!("cannot clean up session {id}: {error}"))
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
                delete,
                ..
            } => {
                if let Err(error) = driver.stop().await {
                    return TaskResult::Failed(format!("cannot close session {id}: {error}"));
                }
                if delete {
                    let target = id.clone();
                    return match tokio::task::spawn_blocking(move || sessions.delete(&target))
                        .await
                        .map_err(std::io::Error::other)
                        .and_then(|result| result)
                    {
                        Ok(()) => TaskResult::Closed,
                        Err(error) => {
                            TaskResult::Failed(format!("cannot delete session {id}: {error}"))
                        }
                    };
                }
                match delete_if_empty(sessions, &id).await {
                    Ok(_) => TaskResult::Closed,
                    Err(error) => TaskResult::Failed(error),
                }
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
                    from,
                } = *opening;
                let (resumed, delete_previous) = match from {
                    OpenFrom::New => (None, None),
                    OpenFrom::Resume(record) => (Some(*record), None),
                    OpenFrom::DeleteCurrent(id) => (None, Some(id)),
                };
                TaskResult::Opened(match opener.open(settings, preferences, resumed).await {
                    Ok(next) => {
                        let mut next = opened(next).await;
                        next.delete_previous = delete_previous;
                        Ok(next)
                    }
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
        self.spawn(ticket, task.run());
    }

    fn spawn(&mut self, ticket: Ticket, result: impl Future<Output = TaskResult> + Send + 'static) {
        self.cancel(ticket.kind);
        let handle = self.set.spawn(async move {
            Done {
                ticket,
                result: result.await,
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
    pub(super) async fn finish(mut self, sessions: Sessions) -> Vec<String> {
        let cancel: Vec<_> = self
            .owner
            .keys()
            .copied()
            .filter(|kind| kind.cancel_on_quit())
            .collect();
        for kind in cancel {
            self.cancel(kind);
        }
        let mut failures = Vec::new();
        while let Some(joined) = self.set.join_next().await {
            match joined {
                Ok(Done {
                    result: TaskResult::Opened(Ok(mut opened)),
                    ..
                }) => {
                    if let Err(reason) = crate::finish_session(&mut opened.session).await {
                        failures.push(format!("session {}: {reason}", opened.id));
                    } else if let Err(error) = delete_if_empty(sessions.clone(), &opened.id).await {
                        failures.push(error);
                    }
                }
                Ok(Done {
                    result: TaskResult::Deleted(id, Err(error)),
                    ..
                }) => failures.push(format!("cannot delete session {id}: {error}")),
                Ok(Done {
                    result: TaskResult::McpToggled(name, Err(error)),
                    ..
                }) => failures.push(format!("MCP {name}: {error}")),
                Ok(Done {
                    result:
                        TaskResult::Opened(Err(error))
                        | TaskResult::Remembered(Err(error))
                        | TaskResult::Failed(error),
                    ..
                }) => failures.push(error),
                Err(error) if !error.is_cancelled() => {
                    failures.push(format!("UI task failed: {error}"))
                }
                _ => {}
            }
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Event;

    #[tokio::test]
    async fn close_reports_record_io_errors() {
        let dir = crate::tool::test_support::TempDir::new("close-error");
        let sessions = Sessions::new(dir.path(), dir.path());
        let ticket = Ticket {
            kind: Kind::Close(1),
            seq: 1,
        };
        let close = |delete| Task::Close {
            ticket,
            driver: super::super::session::Driver::Stopped,
            sessions: sessions.clone(),
            id: "missing-record".into(),
            delete,
        };
        for delete in [false, true] {
            let TaskResult::Failed(error) = close(delete).run().await else {
                panic!("missing record reported as closed")
            };
            assert!(error.contains("missing-record"), "{error}");
            let mut tasks = Tasks::default();
            tasks.start(ticket, close(delete));
            assert_eq!(tasks.finish(sessions.clone()).await, [error]);
        }
    }

    #[tokio::test]
    async fn quit_reports_errors_from_delete_mcp_toggle_and_remember() {
        let dir = crate::tool::test_support::TempDir::new("quit-errors");
        let (opener, settings, _) =
            open_fixture(dir.path(), Provider::named("chatgpt", dir.path()).unwrap());
        let not_a_directory = dir.path().join("file");
        std::fs::write(&not_a_directory, "").unwrap();
        let mut tasks = Tasks::default();
        for task in [
            Task::Delete(opener.sessions.clone(), "missing-session".into()),
            Task::Mcp {
                name: "missing-server".into(),
                config: dir.path().into(),
                registry: Default::default(),
            },
            Task::Remember(not_a_directory.join("choice"), settings),
        ] {
            tasks.start(
                Ticket {
                    kind: task.kind(),
                    seq: 1,
                },
                task,
            );
        }
        let errors = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tasks.finish(opener.sessions),
        )
        .await
        .unwrap();
        assert_eq!(errors.len(), 3, "{errors:?}");
        for expected in ["missing-session", "missing-server", "remember"] {
            assert!(errors.iter().any(|e| e.contains(expected)), "{errors:?}");
        }
    }

    fn open_fixture(
        dir: &std::path::Path,
        provider: Provider,
    ) -> (crate::agent::Opener, Settings, Current) {
        let sessions = Sessions::new(&dir.join("sessions"), dir);
        let opener = crate::agent::Opener {
            sessions: sessions.clone(),
            cwd: dir.into(),
            executable: crate::bart_exe(),
            client: Client::new(),
            store: Store::new(dir.join("auth.json")),
        };
        let settings = Settings {
            provider,
            model: "m".into(),
            reasoning_effort: None,
        };
        let mut preferences = Current::default();
        preferences
            .apply(&crate::preferences::Change::ContextWindow {
                provider: settings.provider.name().into(),
                model: "m".into(),
                tokens: Some(64_000.try_into().unwrap()),
            })
            .unwrap();
        (opener, settings, preferences)
    }

    #[tokio::test]
    async fn explicit_delete_still_requires_the_record_lock_and_handles_an_empty_record() {
        let dir = crate::tool::test_support::TempDir::new("delete-empty");
        let (opener, settings, preferences) =
            open_fixture(dir.path(), Provider::named("chatgpt", dir.path()).unwrap());
        let sessions = opener.sessions.clone();
        let (mut active, id) = opener.open(settings, preferences, None).await.unwrap();
        let close = || Task::Close {
            ticket: Ticket {
                kind: Kind::Close(1),
                seq: 1,
            },
            driver: super::super::session::Driver::Stopped,
            sessions: sessions.clone(),
            id: id.clone(),
            delete: true,
        };
        assert!(matches!(close().run().await, TaskResult::Failed(error) if error.contains(&id)));
        assert!(sessions.dir(&id).is_dir());
        assert_eq!(active.finish().await, Default::default());
        drop(active);
        assert!(matches!(close().run().await, TaskResult::Closed));
        assert!(!sessions.dir(&id).exists());
    }

    #[tokio::test(start_paused = true)]
    async fn close_with_delete_keeps_record_and_outputs_when_job_cleanup_is_unconfirmed() {
        let dir = crate::tool::test_support::TempDir::new("delete-unconfirmed-job");
        let (opener, settings, preferences) =
            open_fixture(dir.path(), Provider::named("chatgpt", dir.path()).unwrap());
        let sessions = opener.sessions.clone();
        let (mut active, id) = opener.open(settings, preferences, None).await.unwrap();
        active
            .rename(Some("retain this session"), |_: Event<'_>| {})
            .await
            .unwrap();
        let output_file = sessions.dir(&id).join("output/retained.txt");
        std::fs::write(&output_file, "still in use").unwrap();
        let hold = active.jobs().hold_for_test();
        let (_commands, rx) = tokio::sync::mpsc::channel(1);
        let (sender, _updates) = super::super::session::channel();
        let driver =
            super::super::session::Driver::new(active, rx, sender, dir.path().join("state"));
        let ticket = Ticket {
            kind: Kind::Close(1),
            seq: 1,
        };
        let result = Task::Close {
            ticket,
            driver,
            sessions: sessions.clone(),
            id: id.clone(),
            delete: true,
        }
        .run()
        .await;
        let TaskResult::Failed(error) = result else {
            panic!("unconfirmed job cleanup must fail close-with-delete");
        };
        assert!(
            error.contains(&id) && error.contains("background jobs"),
            "{error}"
        );
        assert!(sessions.dir(&id).is_dir(), "session record must survive");
        assert_eq!(
            std::fs::read_to_string(&output_file).unwrap(),
            "still in use"
        );
        assert_eq!(
            sessions.resume(&id).unwrap().name.as_deref(),
            Some("retain this session")
        );
        drop(hold);
    }

    #[tokio::test(start_paused = true)]
    async fn quit_preserves_an_empty_session_if_its_close_fails() {
        let dir = crate::tool::test_support::TempDir::new("quit-unconfirmed");
        let (opener, settings, preferences) =
            open_fixture(dir.path(), Provider::named("chatgpt", dir.path()).unwrap());
        let (session, id) = opener.open(settings, preferences, None).await.unwrap();
        let output = opener.sessions.dir(&id).join("output/retained.txt");
        std::fs::write(&output, "still in use").unwrap();
        let _hold = session.jobs().hold_for_test();
        let mut tasks = Tasks::default();
        tasks.spawn(
            Ticket {
                kind: Kind::Open,
                seq: 1,
            },
            async move { TaskResult::Opened(Ok(opened((session, id)).await)) },
        );
        let errors = tasks.finish(opener.sessions).await;
        assert!(
            matches!(&errors[..], [error] if error.contains("background jobs")),
            "{errors:?}"
        );
        assert_eq!(std::fs::read_to_string(output).unwrap(), "still in use");
    }

    #[tokio::test]
    async fn quit_settles_the_open_and_keeps_only_nonempty_records() {
        let dir = crate::tool::test_support::TempDir::new("quit-open");
        let (provider, _) =
            provider::mock::serve(dir.path(), vec![provider::mock::Reply::text("saved")]).await;
        let (opener, settings, preferences) = open_fixture(dir.path(), provider);
        let sessions = opener.sessions.clone();
        let (mut prior, id) = opener
            .open(settings.clone(), preferences.clone(), None)
            .await
            .unwrap();
        prior
            .turn(String::from("keep this session").into(), |_: Event<'_>| {})
            .await
            .unwrap();
        assert_eq!(prior.finish().await, Default::default());
        drop(prior);
        let open_ticket = Ticket {
            kind: Kind::Open,
            seq: 1,
        };
        for existing in [false, true] {
            for stage in 0..3 {
                let mut tasks = Tasks::default();
                let task = Task::Open(Box::new(Opening {
                    opener: opener.clone(),
                    settings: settings.clone(),
                    preferences: preferences.clone(),
                    from: if existing {
                        OpenFrom::Resume(Box::new(sessions.resume(&id).unwrap()))
                    } else if stage < 2 {
                        // Quit must not apply deletion from a replacement it never attached.
                        OpenFrom::DeleteCurrent(id.clone())
                    } else {
                        OpenFrom::New
                    },
                }));
                let (at_gate, waiting) = tokio::sync::oneshot::channel();
                let (release, gate) = tokio::sync::oneshot::channel();
                let (completed, did_open) = tokio::sync::oneshot::channel();
                tasks.spawn(open_ticket, async move {
                    let result = if stage == 0 {
                        at_gate.send(()).unwrap();
                        gate.await.unwrap();
                        task.run().await
                    } else {
                        let result = task.run().await;
                        at_gate.send(()).unwrap();
                        gate.await.unwrap();
                        result
                    };
                    completed
                        .send(matches!(result, TaskResult::Opened(Ok(_))))
                        .unwrap();
                    result
                });
                waiting.await.unwrap();
                if stage == 2 {
                    release.send(()).unwrap();
                    let TaskResult::Opened(Ok(opened)) = tasks.next().await.unwrap().result else {
                        panic!()
                    };
                    let (commands, rx) = tokio::sync::mpsc::channel(1);
                    let (sender, _updates) = super::super::session::channel();
                    let driver = super::super::session::Driver::new(
                        opened.session,
                        rx,
                        sender,
                        dir.path().join("state"),
                    );
                    let ticket = Ticket {
                        kind: Kind::Close(2),
                        seq: 2,
                    };
                    tasks.start(
                        ticket,
                        Task::Close {
                            ticket,
                            driver,
                            sessions: sessions.clone(),
                            id: opened.id,
                            delete: false,
                        },
                    );
                    assert_eq!(tasks.finish(sessions.clone()).await, Vec::<String>::new());
                    drop(commands);
                } else {
                    let finish = tasks.finish(sessions.clone());
                    tokio::pin!(finish);
                    tokio::select! {biased; _ = &mut finish => panic!("quit abandoned the open"), () = std::future::ready(()) => {}}
                    release.send(()).unwrap();
                    assert_eq!(finish.await, Vec::<String>::new());
                }
                assert!(
                    did_open.await.unwrap(),
                    "open must complete at stage {stage}"
                );
                assert_eq!(sessions.list_all().unwrap().len(), 1);
                assert_eq!(sessions.latest().unwrap().as_ref(), Some(&id));
                let (mut reopened, _) = opener
                    .open(
                        settings.clone(),
                        preferences.clone(),
                        Some(sessions.resume(&id).unwrap()),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    reopened.journal().user_messages()[0].1.as_ref(),
                    "keep this session"
                );
                assert_eq!(reopened.finish().await, Default::default());
            }
        }

        let next = opener.open(settings, preferences, None).await.unwrap();
        let empty_id = next.1.clone();
        let mut tasks = Tasks::default();
        let opened = opened(next).await;
        tasks.spawn(open_ticket, async move { TaskResult::Opened(Ok(opened)) });
        let record_dir = sessions.dir(&empty_id);
        let unavailable = dir.path().join("temporarily-unavailable");
        std::fs::rename(&record_dir, &unavailable).unwrap();
        let failures = tasks.finish(sessions.clone()).await;
        std::fs::rename(&unavailable, &record_dir).unwrap();
        assert!(
            matches!(&failures[..], [error] if error.contains(&empty_id)),
            "{failures:?}"
        );
        assert!(!sessions.delete_if_empty(&empty_id).unwrap());
    }
}
