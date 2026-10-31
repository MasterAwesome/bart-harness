use super::tasks::Ticket;
use super::{app::merge, transcript::Transcript};
use crate::agent::Reporter as _;
use crate::{
    agent::{Event as AgentEvent, INPUT_FULL, Inbox, Input, Session, Settings},
    preferences::{Change, Current},
    provider::Delta,
};
use crate::{
    history::Notification,
    tool::{Diff, Status},
};
use std::time::Duration;
use std::{io, sync::Arc};
use tokio::sync::mpsc;

#[derive(Clone)]
pub(super) struct Control(tokio::sync::watch::Sender<Option<Signal>>);

#[derive(Clone, Copy)]
enum Signal {
    Interrupt { resend: bool },
    Quit,
}

impl Control {
    pub(super) fn new() -> Self {
        Self(tokio::sync::watch::channel(None).0)
    }
    pub(super) fn interrupt(&self, resend: bool) {
        self.0.send_replace(Some(Signal::Interrupt { resend }));
    }
    pub(super) fn quit(&self) {
        self.0.send_replace(Some(Signal::Quit));
    }
}

pub(super) enum Command {
    Submit(Input),
    Compact,
    Rename(String),
    Switch(Settings),
    Prefer(Change, Ticket),
    Messages,
    Rewind(usize),
}

pub(super) enum UiEvent {
    SkillAttached,
    RegistryReplaced,
    Started,
    Accounting(crate::agent::Accounting),
    Limits(crate::provider::Limits),
    Renamed(Result<String, String>),
    Switched(Result<Settings, String>),
    Preferred(Ticket, Result<(Box<Current>, Option<String>), String>),
    Notice(String),
    Ended(Option<String>),
    WindowUnknown,
    Interrupted,
    Resent(Input),
    Messages(Vec<(usize, Arc<str>)>),
    Rewound(Result<(String, Box<Transcript>, crate::agent::Accounting), String>),

    MessageBoundary,
    Review(crate::approval::Review),
    Approval(crate::approval::Answer),
    Reasoning(String),
    Text(String),
    ToolBytes(usize),
    Image(std::path::PathBuf),
    ToolStarted {
        id: String,
        name: String,
        arguments: crate::history::Arguments,
        live: Option<u64>,
    },
    ToolFinished {
        id: String,
        output: Arc<str>,
        status: Status,
        jobs: Vec<u32>,
        elapsed: Option<Duration>,
        diffs: Vec<Diff>,
        activities: crate::tool::Activities,
    },
    Compacting,
    CompactingBytes(usize),
    CompactionStage(usize, usize),
    CompactionText(String),
    CompactionStageDone,
    CompactionNoop,
    Compacted(String),
    Responded,
    Notification(Notification),
    Steered(crate::agent::Input),
    WaitingForReset {
        message: String,
        delay: Duration,
    },
    Retrying {
        message: String,
        delay: Duration,
        discarded: bool,
    },
    RecordingStopped(String),
}

#[derive(Clone)]
pub(super) struct Sender {
    pub(super) control: Control,
    queue: mpsc::Sender<Vec<UiEvent>>,
}
pub(super) type Receiver = mpsc::Receiver<Vec<UiEvent>>;
pub(super) fn channel() -> (Sender, Receiver) {
    let (queue, receiver) = mpsc::channel(8);
    (
        Sender {
            control: Control::new(),
            queue,
        },
        receiver,
    )
}
impl Sender {
    async fn send(&self, event: UiEvent) {
        let _ = self.queue.send(vec![event]).await;
    }
}

struct Reporter<'a> {
    updates: &'a Sender,
    pending: Vec<UiEvent>,
}
impl<'a> Reporter<'a> {
    fn new(updates: &'a Sender) -> Self {
        Self {
            updates,
            pending: Vec::new(),
        }
    }
}
impl crate::agent::Reporter for &mut Reporter<'_> {
    fn event(&mut self, event: AgentEvent<'_>) {
        let event = UiEvent::from(event);
        match (self.pending.last_mut(), event) {
            (Some(UiEvent::Text(before)), UiEvent::Text(text))
            | (Some(UiEvent::Reasoning(before)), UiEvent::Reasoning(text))
            | (Some(UiEvent::CompactionText(before)), UiEvent::CompactionText(text)) => {
                before.push_str(&text)
            }
            (_, event) => self.pending.push(event),
        }
    }
    async fn flush(&mut self) {
        if !self.pending.is_empty() {
            let _ = self
                .updates
                .queue
                .send(std::mem::take(&mut self.pending))
                .await;
        }
    }
}

pub(super) enum Driver {
    Running {
        task: tokio::task::JoinHandle<()>,
        control: Control,
    },
    Stopped,
}
impl Driver {
    pub(super) fn new(
        session: Session,
        commands: mpsc::Receiver<Command>,
        updates: Sender,
        state: std::path::PathBuf,
    ) -> Self {
        let control = updates.control.clone();
        Self::Running {
            task: tokio::spawn(drive(session, commands, updates, state)),
            control,
        }
    }
    pub(super) async fn stop(&mut self) {
        if let Self::Running { task, control } = self {
            control.quit();
            let _ = task.await;
        }
        *self = Self::Stopped;
    }
    pub(super) async fn ended(&mut self) -> io::Error {
        let result = match self {
            Self::Running { task, .. } => task.await,
            Self::Stopped => std::future::pending().await,
        };
        *self = Self::Stopped;
        io::Error::other(match result {
            Ok(()) => "agent session ended unexpectedly".to_owned(),
            Err(error) => format!("agent session failed: {error}"),
        })
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        if let Self::Running { task, .. } = self {
            task.abort();
        }
    }
}

enum Work {
    Turn(Input),
    Compact,
    Rename(Option<String>),
    Wake,
}

enum Outcome {
    Interrupt { resend: bool },
    Ended(Option<crate::agent::Error>),
    Quit,
}

pub(super) async fn drive(
    mut session: Session,
    commands: mpsc::Receiver<Command>,
    updates: Sender,
    state: std::path::PathBuf,
) {
    let mut shutdown = updates.control.0.subscribe();
    tokio::select! {
        biased;
        _ = shutdown.wait_for(|signal| matches!(signal, Some(Signal::Quit))) => {}
        () = drive_session(&mut session, commands, updates, state) => {}
    }
    if let Some(reason) = session.finish().await {
        crate::terminal_text::stderr_line(format_args!("bart: {reason}"));
    }
}

async fn drive_session(
    session: &mut Session,
    mut commands: mpsc::Receiver<Command>,
    updates: Sender,
    state: std::path::PathBuf,
) {
    session.prewarm().await;
    let inbox: Inbox = session.inbox();
    let mut control = updates.control.0.subscribe();
    let mut next = None;
    let mut named = session.journal().name().is_some();
    loop {
        let work = if let Some(work) = next.take() {
            work
        } else {
            let Some(work) = idle(session, &mut commands, &updates, &state, &mut control).await
            else {
                break;
            };
            work
        };
        let compacting = matches!(work, Work::Compact);
        let turn = matches!(work, Work::Turn(_));
        let mut report = Reporter::new(&updates);
        report.pending.push(UiEvent::Started);
        let outcome = {
            let turn = async {
                (&mut report).flush().await;
                match work {
                    Work::Turn(input) => session.turn(input, &mut report).await,
                    Work::Compact => session.compact(&mut report).await,
                    Work::Wake => session.wake(&mut report).await,
                    Work::Rename(name) => {
                        let result = session.rename(name.as_deref(), &mut report).await;
                        // A failed automatic name leaves the session unnamed without a message.
                        if let (Some(_), Err(error)) = (&name, result) {
                            report.pending.push(UiEvent::Renamed(Err(error)));
                        }
                        Ok(())
                    }
                }
            };
            tokio::select! {
                biased;
                () = async { let _ = control.changed().await; } => match *control.borrow_and_update() {
                    Some(Signal::Interrupt { resend }) => Outcome::Interrupt { resend },
                    Some(Signal::Quit) | None => Outcome::Quit,
                },
                result = turn => Outcome::Ended(result.err()),
                outcome = interruption(&mut commands, &inbox, &updates) => outcome,
            }
        };
        if !matches!(outcome, Outcome::Quit) {
            (&mut report).flush().await;
        }
        let resend = match outcome {
            Outcome::Ended(error) => {
                let successful = error.is_none();
                if successful && turn && !named {
                    named = true;
                    if session.journal().name().is_none() {
                        next = Some(Work::Rename(None));
                    }
                }
                if matches!(error, Some(crate::agent::Error::UnknownWindow(_))) {
                    let () = updates.send(UiEvent::WindowUnknown).await;
                }
                let () = updates
                    .send(UiEvent::Ended(error.map(|error| error.to_string())))
                    .await;
                compacting && successful
            }
            Outcome::Interrupt { resend } => {
                let () = updates.send(UiEvent::Interrupted).await;
                resend
            }
            Outcome::Quit => return,
        };
        if resend {
            let steers = inbox.take();
            if !steers.is_empty() {
                let input = merge(steers, "\n\n");
                let () = updates.send(UiEvent::Resent(input.clone())).await;
                next = Some(Work::Turn(input));
            }
        }
    }
}

async fn idle(
    session: &mut Session,
    commands: &mut mpsc::Receiver<Command>,
    updates: &Sender,
    state: &std::path::Path,
    control: &mut tokio::sync::watch::Receiver<Option<Signal>>,
) -> Option<Work> {
    loop {
        return Some(tokio::select! {
            biased;
            () = async { let _ = control.changed().await; } => {
                if matches!(*control.borrow_and_update(), Some(Signal::Quit)) { return None; }
                continue;
            }
            command = commands.recv() => match command {
                Some(Command::Submit(input)) => Work::Turn(input),
                Some(Command::Compact) => Work::Compact,
                Some(Command::Rename(name)) => Work::Rename(Some(name)),
                Some(Command::Prefer(change, ticket)) => {
                    let mut report = Reporter::new(updates);
                    let before = session.tool_context().mcp();
                    let result = session.prefer(&change, &mut report).await;
                    if !Arc::ptr_eq(&before, &session.tool_context().mcp()) { report.pending.push(UiEvent::RegistryReplaced); }
                    (&mut report).flush().await;
                    let () = updates.send(UiEvent::Preferred(ticket, result.map(|(preferences, warning)| (Box::new(preferences), warning)))).await;
                    continue;
                }
                Some(Command::Switch(settings)) => {
                    switch(session, settings, updates, state).await;
                    continue;
                }
                Some(Command::Messages) => {
                    { let mut report = Reporter::new(updates); session.settle_recording(&mut report).await; (&mut report).flush().await; }
                    let () = updates.send(UiEvent::Messages(session.journal().user_messages())).await;
                    continue;
                }

                Some(Command::Rewind(at)) => {
                    let () = updates.send(rewind(session, at, updates).await).await;
                    continue;
                }
                None => return None,
            },
            () = session.jobs_ready(), if session.has_jobs() => Work::Wake,
        });
    }
}

async fn switch(
    session: &mut Session,
    settings: Settings,
    updates: &Sender,
    state: &std::path::Path,
) {
    let mut report = Reporter::new(updates);
    session.switch(settings.clone(), &mut report).await;
    (&mut report).flush().await;
    if let Err(warning) = super::remember(state, &settings).await {
        let () = updates.send(UiEvent::Notice(warning)).await;
    }
    let () = updates.send(UiEvent::Switched(Ok(settings))).await;
}

async fn rewind(session: &mut Session, at: usize, updates: &Sender) -> UiEvent {
    let mut report = Reporter::new(updates);
    let result = session.rewind(at, &mut report).await;
    (&mut report).flush().await;
    match result {
        Ok(text) => {
            let transcript = super::transcript::project_rewind(session).await;
            UiEvent::Rewound(Ok((text, Box::new(transcript), session.accounting())))
        }
        Err(error) => UiEvent::Rewound(Err(error)),
    }
}

async fn interruption(
    commands: &mut mpsc::Receiver<Command>,
    inbox: &Inbox,
    updates: &Sender,
) -> Outcome {
    loop {
        match commands.recv().await {
            None => return Outcome::Quit,
            Some(Command::Submit(input)) => {
                if inbox.push(input).is_err() {
                    let () = updates.send(UiEvent::Notice(INPUT_FULL.into())).await;
                }
            }
            Some(command) => {
                let () = updates.send(refused(command)).await;
            }
        }
    }
}

fn refused(command: Command) -> UiEvent {
    match command {
        Command::Rename(_) => UiEvent::Renamed(Err("cannot rename during a turn".into())),
        Command::Compact => UiEvent::Notice(
            "cannot compact while the model is running; interrupt it and try again".into(),
        ),
        Command::Rewind(_) => UiEvent::Rewound(Err(
            "cannot rewind while the model is running; interrupt it and try again".into(),
        )),
        Command::Prefer(_, ticket) => UiEvent::Preferred(
            ticket,
            Err("cannot change settings during a turn; Esc cancels the wait".into()),
        ),
        Command::Switch(_) => UiEvent::Switched(Err(
            "cannot switch models while the model is running; interrupt it and try again".into(),
        )),
        Command::Messages => UiEvent::Notice(
            "cannot open rewind while the model is running; interrupt it and try again".into(),
        ),
        Command::Submit(_) => UiEvent::Notice(INPUT_FULL.into()),
    }
}

impl From<AgentEvent<'_>> for UiEvent {
    fn from(event: AgentEvent<'_>) -> Self {
        match event {
            AgentEvent::Renamed(name) => UiEvent::Renamed(Ok(name.to_owned())),
            AgentEvent::SkillAttached(_) => UiEvent::SkillAttached,
            AgentEvent::Accounting(value) => UiEvent::Accounting(value),
            AgentEvent::ProviderObservation(crate::provider::Observation::Limits(value)) => {
                UiEvent::Limits(value)
            }
            AgentEvent::Review(review) => UiEvent::Review(review.clone()),
            AgentEvent::Image(image) => UiEvent::Image(image.path.clone()),
            AgentEvent::Approval(answer) => UiEvent::Approval(answer.clone()),
            AgentEvent::Delta(Delta::MessageBoundary) => UiEvent::MessageBoundary,
            AgentEvent::Delta(Delta::Text(text)) => UiEvent::Text(text.to_owned()),
            AgentEvent::Delta(Delta::Reasoning(text)) => UiEvent::Reasoning(text.to_owned()),
            AgentEvent::Delta(Delta::ToolBytes(bytes)) => UiEvent::ToolBytes(bytes),
            AgentEvent::ToolStarted { call, live } => UiEvent::ToolStarted {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                live: Some(live),
            },
            AgentEvent::ToolFinished {
                call,
                output,
                elapsed,
            } => UiEvent::ToolFinished {
                id: call.id.clone(),
                output: output.text.clone().into(),
                status: output.status,
                jobs: output.jobs.clone(),
                elapsed: Some(elapsed),
                diffs: output.diffs.clone(),
                activities: output.activities.clone(),
            },
            AgentEvent::Compacting => UiEvent::Compacting,
            AgentEvent::CompactingBytes(bytes) => UiEvent::CompactingBytes(bytes),
            AgentEvent::CompactionStage { index, total } => UiEvent::CompactionStage(index, total),
            AgentEvent::CompactionStageDone => UiEvent::CompactionStageDone,
            AgentEvent::CompactionNoop => UiEvent::CompactionNoop,
            AgentEvent::CompactionText(text) => UiEvent::CompactionText(text.to_owned()),
            AgentEvent::Compacted { summary, .. } => UiEvent::Compacted(summary.to_owned()),
            AgentEvent::Responded(_) => UiEvent::Responded,
            AgentEvent::Notification(notification) => UiEvent::Notification(notification.clone()),
            AgentEvent::Steered(text) => UiEvent::Steered(text.to_owned()),
            AgentEvent::RecordingStopped(reason) => UiEvent::RecordingStopped(reason.to_owned()),
            AgentEvent::WaitingForReset { error, delay } => UiEvent::WaitingForReset {
                message: error.to_string(),
                delay,
            },
            AgentEvent::Retrying {
                error,
                delay,
                discarded,
            } => UiEvent::Retrying {
                message: error.to_string(),
                delay,
                discarded,
            },
        }
    }
}
