//! A cancelled turn's calls are answered as interrupted with unknown effects: providers reject
//! unanswered calls, and nothing may claim an interrupted call had no effect.
mod compact;
mod handoff;
mod input;
mod journal;
mod open;
mod prompt;
mod request;
mod routing;
mod skills;
mod tools;
pub(crate) use handoff::checkpoint_summary;
pub(crate) use input::{FULL as INPUT_FULL, Inbox, Input, InputSize, fits as inputs_fit};
pub(crate) use journal::Journal;
pub(crate) use open::Opener;
use prompt::Instructions;
use routing::Routing;

use std::{fmt, path::Path, sync::Arc, time::Duration};

use reqwest::Client;

use crate::{
    auth::Store,
    history::{Assistant, Content, Message, Notification, NotificationKind, ToolCall, Usage},
    preferences::{Change, Current},
    provider::{self, Delta, Provider},
    record::{Meta, Recorder},
    tool::{self, Confinement, Context, Output},
};

const MAX_RESPONSE_IMAGES: usize = 8;
const INTERRUPTED: &str = "interrupted before a result was recorded; any effects are unknown. Python state is kept unless the next result reports a restart.";
const WAKE_COALESCING: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) provider: Provider,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
}

impl Settings {
    pub(crate) fn choice(&self) -> crate::choice::Choice {
        crate::choice::Choice {
            provider: self.provider.name().to_owned(),
            model: self.model.clone(),
            effort: self.reasoning_effort.clone(),
        }
    }
}

pub(crate) trait Reporter: Send {
    fn event(&mut self, event: Event<'_>);
    fn flush(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

impl<F: FnMut(Event<'_>) + Send> Reporter for F {
    fn event(&mut self, event: Event<'_>) {
        self(event);
    }

    async fn flush(&mut self) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Accounting {
    pub(crate) context_tokens: u64,
    pub(crate) usage: crate::conversation::UsageSummary,
}

#[derive(Clone, Copy)]
pub(crate) enum Event<'a> {
    Delta(Delta<'a>),
    ProviderObservation(provider::Observation),
    Review(&'a crate::approval::Review),
    Approval(&'a crate::approval::Answer),
    Compacting,
    CompactingBytes(usize),
    CompactionStage {
        index: usize,
        total: usize,
    },
    CompactionText(&'a str),
    CompactionStageDone,
    CompactionNoop,
    Compacted {
        report: &'a str,
        summary: &'a str,
    },
    ToolStarted {
        call: &'a ToolCall,
        live: u64,
    },
    ToolFinished {
        call: &'a ToolCall,
        output: &'a Output,
        elapsed: Duration,
    },
    Responded(Option<Usage>),
    Accounting(Accounting),
    Notification(&'a Notification),
    Steered(&'a Input),
    SkillAttached(&'a crate::history::Skill),
    Image(&'a crate::history::Image),
    RecordingStopped(&'a str),
    Renamed(&'a str),
    WaitingForReset {
        error: &'a provider::Error,
        delay: Duration,
    },
    Retrying {
        error: &'a provider::Error,
        delay: Duration,
        discarded: bool,
    },
}

#[derive(Debug)]
pub(crate) enum Error {
    Credential(String),
    Model(provider::Error),
    Unsupported(String),
    Context(String),
    UnknownWindow(String),
    Overflow {
        error: provider::Error,
        discarded: bool,
    },
    NotApproved,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Credential(problem)
            | Self::Unsupported(problem)
            | Self::Context(problem)
            | Self::UnknownWindow(problem) => f.write_str(problem),
            Self::NotApproved => f.write_str("turn stopped: an action was not approved"),
            Self::Model(error) => error.fmt(f),
            Self::Overflow { error, .. } => write!(
                f,
                "{error}; it still does not fit after compacting; rewind or start a new session"
            ),
        }
    }
}

impl std::error::Error for Error {}

pub(crate) struct Session {
    routing: Routing,
    context: Arc<Context>,
    instructions: Instructions,
    context_tokens: u64,
    cache_key: String,
    journal: Journal,
    inbox: Inbox,
    interactive: bool,
    reviewer: crate::approval::Reviewer,
}

impl Settings {
    #[must_use]
    pub(crate) fn meta(&self, cwd: &Path) -> Meta {
        Meta {
            cwd: cwd.to_path_buf(),
            provider: self.provider.name().to_owned(),
            model: self.model.clone(),
            effort: self.reasoning_effort.clone(),
        }
    }

    #[must_use]
    pub(crate) fn describe(&self, confinement: Confinement) -> String {
        let effort = self
            .reasoning_effort
            .as_ref()
            .map_or_else(String::new, |effort| format!(" · {effort} effort"));
        format!(
            "{} · {}{effort} · {}",
            self.provider.name(),
            self.model,
            confinement.label()
        )
    }
}

impl Session {
    #[must_use]
    pub(crate) fn new(
        client: Client,
        store: Store,
        settings: Settings,
        preferences: Current,
        mut context: Context,
        cache_key: String,
        record: Option<Recorder>,
    ) -> Self {
        if let Some(record) = &record {
            context.bind_artifacts(record.weak_writer());
        }

        if record.is_some()
            && let Err(error) = context.record_diagnostics()
        {
            crate::terminal_text::stderr_line(format_args!(
                "bart: diagnostic logging unavailable: {error}"
            ));
        }
        let routing = Routing::new(client, store, settings, preferences, context.cwd.clone());
        context.diffs.store(
            routing.preferences().effective.diffs,
            std::sync::atomic::Ordering::Relaxed,
        );
        context.route_web(routing.web_route(context.diagnostics.clone()));
        context.identify(
            routing.settings().provider.name(),
            &routing.settings().model,
            routing.settings().reasoning_effort.as_deref(),
        );
        let instructions = Instructions::load(&context, record.as_ref());
        let reviewer = crate::approval::Reviewer::new(&cache_key);
        Self {
            routing,
            context: Arc::new(context),
            context_tokens: compact::base(instructions.as_str()),
            instructions,
            cache_key,
            journal: Journal::new(record),
            inbox: Inbox::default(),
            interactive: false,
            reviewer,
        }
    }

    pub(crate) async fn rename(
        &mut self,
        requested: Option<&str>,
        mut on_event: impl Reporter,
    ) -> Result<String, String> {
        let title = if let Some(title) = requested {
            title.to_owned()
        } else {
            self.settle_recording_with(&mut on_event).await;
            self.routing.entry().await;
            let first = self
                .journal
                .history()
                .iter()
                .find_map(|message| match message {
                    Message::User(text) => Some(text.clone()),
                    _ => None,
                })
                .ok_or("session has no user prompt")?;
            let history = [Message::User(first), Message::User(request::NAME.into())];
            let mut request = self.routing.request(&history, &[], "", None);
            request.tools = provider::Tools::Disabled(&[]);
            request::name(
                &self.routing,
                self.context.diagnostics.as_ref(),
                request,
                &mut on_event,
            )
            .await?
        };
        if title.trim().is_empty()
            || title.chars().count() > 80
            || title.chars().any(crate::terminal_text::unsafe_character)
        {
            return Err("name must be 1–80 terminal-safe characters on one line".into());
        }
        let title = if requested.is_none() {
            self.journal.automatic_name(title.trim()).await?
        } else {
            title
        };
        if title.chars().count() > 80 {
            return Err("name must be 1–80 terminal-safe characters on one line".into());
        }
        self.journal.rename(title.clone()).await;
        on_event.event(Event::Renamed(&title));
        self.settle_recording_with(&mut on_event).await;
        Ok(title.to_owned())
    }

    async fn resume(
        &mut self,
        meta: Option<&Meta>,
        name: Option<String>,
        conversation: crate::conversation::Conversation,
    ) {
        self.journal.resume(meta, name, conversation).await;
        self.rebase_context();
        self.journal.settle_recording().await;
    }

    fn cache(&self) -> provider::Cache<'_> {
        provider::Cache {
            key: &self.cache_key,
            window: self.journal.compactions(),
        }
    }

    pub(crate) fn accounting(&self) -> Accounting {
        Accounting {
            context_tokens: self.context_tokens,
            usage: self.journal.usage(),
        }
    }

    pub(crate) fn journal(&self) -> &Journal {
        &self.journal
    }

    pub(crate) async fn switch(&mut self, settings: Settings, mut on_event: impl Reporter) {
        self.journal
            .describe(&settings.meta(&self.context.cwd))
            .await;
        self.apply_switch(settings);
        on_event.event(Event::Accounting(self.accounting()));
        self.settle_recording_with(&mut on_event).await;
    }

    fn refresh_instructions(&mut self) {
        self.instructions.refresh(&self.context);
        self.rebase_context();
    }

    fn rebase_context(&mut self) {
        self.context_tokens =
            compact::tokens(self.journal.model_context(), self.instructions.as_str());
    }

    fn apply_switch(&mut self, settings: Settings) {
        self.routing.select(settings);
        self.context.identify(
            self.routing.settings().provider.name(),
            &self.routing.settings().model,
            self.routing.settings().reasoning_effort.as_deref(),
        );
        self.context
            .route_web(self.routing.web_route(self.context.diagnostics.clone()));
        self.refresh_instructions();
    }

    pub(crate) async fn rewind(
        &mut self,
        at: usize,
        mut on_event: impl Reporter,
    ) -> Result<String, String> {
        if !matches!(self.journal.history().get(at), Some(Message::User(_))) {
            return Err("that message is no longer in the conversation".into());
        }
        let evidence = match self.archive_context(&self.journal.history()[at..]).await {
            Ok(number) => {
                format!("removed messages: history[{number}]")
            }
            Err(error) => format!("removed messages were not saved: {error}"),
        };
        let text = self.journal.rewind(at).await?;
        self.rebase_context();
        on_event.event(Event::Accounting(self.accounting()));
        self.context.jobs.stop_all();
        let cleanup = self.context.jobs.settled().await;
        self.report_jobs(&mut on_event).await;
        self.context.reset_python();
        self.push(Message::Notification(Notification {
            text: bart!(Session, "conversation rewound; Python state is gone, files and earlier effects remain; {}; {evidence}", cleanup.notice()).into(),
            kind: NotificationKind::Recovery,
        }), &mut on_event).await;
        self.settle_recording_with(&mut on_event).await;
        Ok(text.to_string())
    }

    pub(crate) fn interactive(&mut self) {
        self.interactive = true;
    }

    pub(crate) fn preferences(&self) -> Current {
        self.routing.preferences().clone()
    }

    pub(crate) async fn prefer(
        &mut self,
        change: &Change,
        mut on_event: impl Reporter,
    ) -> Result<(Current, Option<String>), String> {
        self.settle_recording_with(&mut on_event).await;
        let routing::Preferred {
            confinement,
            mut warning,
        } = self
            .routing
            .prefer(change, &self.context.cwd, self.context.confinement())
            .await?;
        if let Some((confinement, mcp)) = confinement {
            let mcp = crate::mcp::Registry::start(
                &mcp,
                tool::Place {
                    confinement,
                    ..self.context.place()
                },
                &self.routing.client,
                &self.routing.store,
            );
            let (cleanup, restart) = self.context.reconfine(confinement, mcp).await;
            if let Err(error) = &restart {
                warning = Some(format!(
                    "{}settings applied; Python restart failed: {error}",
                    warning.map_or(String::new(), |warning| format!("{warning}; "))
                ));
            }
            let interpreter = match restart {
                Ok(()) => "interpreter restarted, Python state is gone".into(),
                Err(error) => format!("interpreter restart failed: {error}"),
            };
            self.push(
                Message::Notification(Notification {
                    text: bart!(
                        Session,
                        "sandbox settings changed; {interpreter}; {}",
                        cleanup.notice()
                    )
                    .into(),
                    kind: NotificationKind::Recovery,
                }),
                &mut on_event,
            )
            .await;
        }
        self.context.diffs.store(
            self.routing.preferences().effective.diffs,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.context
            .route_web(self.routing.web_route(self.context.diagnostics.clone()));
        self.refresh_instructions();
        on_event.event(Event::Accounting(self.accounting()));
        Ok((self.routing.preferences().clone(), warning))
    }

    #[must_use]
    pub(crate) fn settings(&self) -> &Settings {
        self.routing.settings()
    }

    #[must_use]
    pub(crate) fn tool_context(&self) -> Arc<Context> {
        Arc::clone(&self.context)
    }

    #[must_use]
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    pub(crate) fn jobs(&self) -> Arc<tool::Jobs> {
        Arc::clone(&self.context.jobs)
    }

    pub(crate) fn live(&self) -> Arc<tool::Live> {
        Arc::clone(&self.context.live)
    }

    pub(crate) async fn turn(
        &mut self,
        mut input: Input,
        mut on_event: impl Reporter,
    ) -> Result<(), Error> {
        input.resolve_skill().map_err(Error::Context)?;
        let held = self
            .inbox
            .reserve(input)
            .ok_or_else(|| Error::Context(INPUT_FULL.into()))?;
        self.routing.entry().await;
        self.compaction_threshold()?;
        self.recover(&mut on_event).await;
        self.add_input(held, &mut on_event).await?;
        on_event.flush().await;
        self.respond(&mut on_event).await
    }

    fn compaction_threshold(&self) -> Result<u64, Error> {
        self.routing.compaction_threshold()
    }

    async fn add_input(
        &mut self,
        held: input::HeldInput,
        on_event: &mut impl Reporter,
    ) -> Result<(), Error> {
        let mut input = held.input().clone();
        input.resolve_skill().map_err(Error::Context)?;
        if !input.images.is_empty() && !self.routing.sees_images() {
            return Err(Error::Unsupported(format!(
                "{} does not accept images",
                self.routing.settings().model
            )));
        }
        let mut messages = vec![Message::User(input.text.into())];
        for image in input.images {
            messages.push(Message::Image(
                self.context
                    .save_image(image)
                    .await
                    .map_err(|error| Error::Context(format!("cannot save image: {error}")))?,
            ));
        }
        messages.extend(input.skills.iter().cloned().map(Message::Skill));
        let bytes = messages
            .iter()
            .map(crate::history::message_len)
            .fold(self.journal.bytes(), usize::saturating_add);
        Self::check_history_size(bytes)?;
        let appended = self.journal.append(messages).await;
        compact::appended(&mut self.context_tokens, appended);
        held.accept();
        for skill in &input.skills {
            on_event.event(Event::SkillAttached(skill));
        }
        on_event.event(Event::Accounting(self.accounting()));
        self.settle_recording_with(on_event).await;
        Ok(())
    }

    pub(crate) async fn compact(&mut self, mut on_event: impl Reporter) -> Result<(), Error> {
        self.compact_with(&mut on_event, 0).await
    }

    pub(super) async fn compact_with(
        &mut self,
        on_event: &mut impl Reporter,
        at_least: u64,
    ) -> Result<(), Error> {
        Self::check_history_size(self.journal.bytes())?;
        on_event.event(Event::Compacting);
        on_event.flush().await;
        self.recover(on_event).await;
        self.report_jobs(on_event).await;
        on_event.flush().await;
        self.routing.entry().await;
        let limit = self.compaction_threshold()?;
        let full_window = self.routing.window()?;
        let before = compact::tokens(self.journal.model_context(), self.instructions.as_str());
        if at_least == 0 && before <= full_window / 3 {
            on_event.event(Event::CompactionNoop);
            return Ok(());
        }
        let snapshot = self.journal.model_context().snapshot();
        let history = snapshot.view().messages().to_vec();
        let inventory = self.context.python_inventory();
        let mut evidence = history.clone();
        evidence.push(Message::Instruction(
            format!(
                "{}\n{inventory}",
                bart!(const Summary, "Python state at this point")
            )
            .into(),
        ));
        let archive = self.archive_context(&evidence).await?;
        let instructions = self.instructions.as_str().to_owned();
        let definitions = tool::definitions(&self.context).await;
        let summary_definitions = definitions.clone();
        let plan = tokio::task::spawn_blocking(move || {
            handoff::Plan::new(
                &history,
                full_window,
                &instructions,
                archive,
                inventory,
                &summary_definitions,
            )
        })
        .await
        .map_err(|e| Error::Context(format!("summary planning failed: {e}")))??;
        let request = self.routing.request(
            &[],
            &definitions,
            self.instructions.as_str(),
            Some(self.cache()),
        );
        let summary = handoff::summarize(&self.routing, &plan, archive, &request, on_event).await?;
        let compact::Reduced { window, estimate } =
            plan.finish(&summary, archive, self.instructions.as_str())?;
        if estimate.tokens >= limit || estimate.tokens >= before.max(at_least) {
            return Err(Error::Context(
                "summary exceeds the context budget; context retained".into(),
            ));
        }

        let report = format!(
            "Compacted context: ~{before} → ~{} tokens.",
            estimate.tokens
        );
        self.journal.checkpoint(window, estimate).await;
        self.rebase_context();
        on_event.event(Event::Accounting(self.accounting()));
        self.settle_recording_with(on_event).await;
        on_event.event(Event::Compacted {
            report: &report,
            summary: checkpoint_summary(self.journal.model_context().messages())
                .unwrap_or(&summary),
        });
        Ok(())
    }

    async fn archive_context(&self, messages: &[Message]) -> Result<u64, Error> {
        let history = messages.to_vec();
        let artifacts = self.context.artifacts();
        tokio::task::spawn_blocking(move || {
            let bytes = serde_json::to_vec(&history).expect("serializable history");
            artifacts.save_history(&bytes)
        })
        .await
        .map_err(|e| Error::Context(format!("archive worker failed: {e}")))?
        .map_err(|e| Error::Context(format!("cannot preserve original history: {e}")))
    }

    fn check_history_size(bytes: usize) -> Result<(), Error> {
        if bytes > journal::MAX_HISTORY_BYTES {
            Err(Error::Context(
                "conversation exceeds the 128 MiB safety limit; rewind or start a new session"
                    .into(),
            ))
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub(crate) fn has_jobs(&self) -> bool {
        self.context.jobs.any()
    }

    pub(crate) async fn settle_recording(&mut self, mut on_event: impl Reporter) {
        self.settle_recording_with(&mut on_event).await;
    }

    async fn settle_recording_with(&mut self, on_event: &mut impl Reporter) {
        self.journal.settle_recording().await;
        if let Some(reason) = self.journal.take_recording_notice() {
            on_event.event(Event::RecordingStopped(reason));
        }
    }

    pub(crate) async fn finish(&mut self) -> Option<&str> {
        self.context.stop();
        self.routing.transport.close().await;
        self.context.jobs.settled().await;
        self.journal
            .finish(|| self.context.jobs.take_notifications())
            .await;
        self.rebase_context();
        self.journal.take_recording_notice()
    }

    pub(crate) async fn jobs_ready(&self) {
        self.context.jobs.wake_ready().await;
    }

    pub(crate) async fn wake(&mut self, mut on_event: impl Reporter) -> Result<(), Error> {
        tokio::time::sleep(WAKE_COALESCING).await;
        self.respond(&mut on_event).await
    }

    pub(crate) async fn prewarm(&mut self) {
        self.routing.entry().await;
        self.refresh_instructions();
        let definitions = tool::definitions(&self.context).await;
        let request = self.routing.request(
            self.journal.model_context().messages(),
            &definitions,
            self.instructions.as_str(),
            Some(self.cache()),
        );
        let Ok(credential) =
            provider::credential(&self.routing.store, &self.routing.client, request.provider).await
        else {
            return;
        };
        let _ = self
            .routing
            .transport
            .prewarm(credential.as_ref(), &request)
            .await;
    }

    async fn respond(&mut self, on_event: &mut impl Reporter) -> Result<(), Error> {
        self.routing
            .transport
            .begin_turn()
            .await
            .map_err(Error::Model)?;
        let definitions = tool::definitions(&self.context).await;
        let mut recovered = false;
        loop {
            self.recover(on_event).await;
            self.report_jobs(on_event).await;
            self.routing.entry().await;
            while let Some(held) = self.inbox.next() {
                let input = held.input().clone();
                self.add_input(held, on_event).await?;
                on_event.event(Event::Steered(&input));
            }
            self.refresh_instructions();
            Self::check_history_size(self.journal.bytes())?;
            let accounting = self.accounting();
            on_event.event(Event::Accounting(accounting));
            let continuing = self
                .journal
                .model_context()
                .messages()
                .iter()
                .rev()
                .find_map(|m| {
                    if let Message::Assistant(a) = m {
                        Some(a.continues)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            if accounting.context_tokens >= self.compaction_threshold()? && !continuing {
                self.compact_with(on_event, 0).await?;
            }
            let request = self.routing.request(
                self.journal.model_context().messages(),
                &definitions,
                self.instructions.as_str(),
                Some(self.cache()),
            );
            on_event.flush().await;
            let assistant = match request::respond(
                &self.routing,
                self.context.diagnostics.as_ref(),
                &request,
                on_event,
            )
            .await
            {
                Err(Error::Overflow { error, discarded }) if !recovered && !continuing => {
                    recovered = true;
                    on_event.event(Event::Retrying {
                        error: &error,
                        delay: Duration::ZERO,
                        discarded,
                    });
                    let window = self.routing.window()?;
                    self.compact_with(on_event, window).await?;
                    continue;
                }
                result => result?,
            };
            on_event.event(Event::Responded(assistant.usage));
            let calls = tool_calls(&assistant);
            let continues = assistant.continues;
            let refused = assistant.refused;
            self.push(Message::Assistant(assistant), on_event).await;
            if refused
                || (calls.is_empty()
                    && !continues
                    && self.inbox.is_empty()
                    && !self.context.jobs.has_notifications())
            {
                return Ok(());
            }
            Self::check_history_size(self.journal.bytes())?;
            on_event.flush().await;
            if self.run_calls(&calls, on_event).await? {
                return Err(Error::NotApproved);
            }
            self.journal.answered();
        }
    }

    async fn recover(&mut self, on_event: &mut impl Reporter) {
        if self.journal.recover(INTERRUPTED).await {
            self.rebase_context();
            on_event.event(Event::Accounting(self.accounting()));
        }
        self.settle_recording_with(on_event).await;
    }

    async fn report_jobs(&mut self, on_event: &mut impl Reporter) {
        // Do not drain notifications while append could still await an earlier write.
        self.settle_recording_with(on_event).await;
        let jobs = self.context.jobs.take_notifications();
        let appended = self
            .journal
            .append(jobs.into_iter().map(Message::Notification).collect())
            .await;
        compact::appended(&mut self.context_tokens, appended);
        let changed = !appended.is_empty();
        for message in appended {
            if let Message::Notification(job) = message {
                on_event.event(Event::Notification(job));
            }
        }
        if changed {
            on_event.event(Event::Accounting(self.accounting()));
        }
        self.settle_recording_with(on_event).await;
    }

    async fn push(&mut self, message: Message, on_event: &mut impl Reporter) {
        let appended = self.journal.append(vec![message]).await;
        compact::appended(&mut self.context_tokens, appended);
        for message in appended {
            match message {
                Message::Image(image) => on_event.event(Event::Image(image)),
                Message::Notification(note) => on_event.event(Event::Notification(note)),
                _ => {}
            }
        }
        on_event.event(Event::Accounting(self.accounting()));
        self.settle_recording_with(on_event).await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.context.stop();
        self.journal
            .close_on_drop(self.context.jobs.take_notifications());
    }
}

struct ReviewReporter<'a, R> {
    reporter: &'a mut R,
}
impl<R: Reporter> crate::approval::Report for ReviewReporter<'_, R> {
    fn notice(&mut self, notice: crate::approval::Notice<'_>) {
        self.reporter.event(match notice {
            crate::approval::Notice::Review(review) => Event::Review(review),
            crate::approval::Notice::Ask(answer) => Event::Approval(answer),
            crate::approval::Notice::RecordingStopped(reason) => Event::RecordingStopped(reason),
        });
    }
    fn flush(&mut self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.reporter.flush())
    }
}

fn tool_calls(assistant: &Assistant) -> Vec<ToolCall> {
    assistant
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            Content::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
