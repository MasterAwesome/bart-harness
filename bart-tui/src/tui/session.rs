use super::tasks::Ticket;
use super::{app::merge, transcript::Transcript};
use crate::agent::Reporter as _;
use crate::{
    agent::{Event as AgentEvent, Inbox, Input, Session, Settings},
    preferences::{Change, Current},
    provider::Delta,
};
use crate::{
    history::Notification,
    tool::{Diff, Status},
};
use std::time::Duration;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub(super) struct Control {
    signal: tokio::sync::watch::Sender<Option<Signal>>,
    input: Arc<InputStart>,
}

#[derive(Default)]
struct InputStart {
    // Serialize TUI starts, recalls and claims. Core sees only the input queue.
    ready: Mutex<Ready>,
}

#[derive(Default)]
struct Ready {
    next: u64,
    active: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resume {
    Empty,
    Started,
    Refused,
}

#[derive(Clone, Copy)]
enum Signal {
    Interrupt { resend: bool },
    Quit,
}

impl Control {
    pub(super) fn new() -> Self {
        Self {
            signal: tokio::sync::watch::channel(None).0,
            input: Default::default(),
        }
    }
    pub(super) fn enqueue(
        &self,
        inbox: &Inbox,
        commands: &mpsc::Sender<Command>,
        input: Input,
        start: bool,
    ) -> bool {
        let mut ready = self.input.ready.lock().unwrap();
        let permit = if start {
            let Ok(permit) = commands.try_reserve() else {
                return false;
            };
            Some(permit)
        } else {
            None
        };
        if inbox.push(input).is_err() {
            return false;
        }
        if let Some(permit) = permit {
            ready.next += 1;
            let token = ready.next;
            ready.active = Some(token);
            permit.send(Command::StartTurn(token));
        }
        true
    }
    pub(super) fn recall(&self, inbox: &Inbox, input: &Input) -> bool {
        let mut ready = self.input.ready.lock().unwrap();
        let removed = inbox.remove(input).is_some();
        if inbox.is_empty() {
            ready.active = None;
        }
        removed
    }
    pub(super) fn resume_input(&self, inbox: &Inbox, commands: &mpsc::Sender<Command>) -> Resume {
        let mut ready = self.input.ready.lock().unwrap();
        if inbox.is_empty() {
            ready.active = None;
            return Resume::Empty;
        }
        let Ok(permit) = commands.try_reserve() else {
            ready.active = None;
            return Resume::Refused;
        };
        ready.next += 1;
        let token = ready.next;
        ready.active = Some(token);
        permit.send(Command::StartTurn(token));
        Resume::Started
    }
    pub(super) fn recover_input(&self, inbox: &Inbox) -> Vec<Input> {
        let mut ready = self.input.ready.lock().unwrap();
        ready.active = None;
        inbox.take()
    }
    fn claim_input(&self, inbox: &Inbox, token: u64) -> Vec<Input> {
        let mut ready = self.input.ready.lock().unwrap();
        if ready.active == Some(token) {
            ready.active = None;
            inbox.take()
        } else {
            Vec::new()
        }
    }
    pub(super) fn interrupt(&self, resend: bool) {
        self.signal.send_replace(Some(Signal::Interrupt { resend }));
    }
    pub(super) fn quit(&self) {
        self.signal.send_replace(Some(Signal::Quit));
    }
}

pub(super) enum Command {
    StartTurn(u64),
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
    Started(Vec<Input>),
    Accounting(crate::agent::Accounting),
    Limits(crate::provider::Limits),
    Renamed(Result<String, String>),
    Switched(Result<Settings, String>),
    Preferred(Ticket, Result<(Box<Current>, Option<String>), String>),
    Notice(String),
    Ended {
        ack: tokio::sync::oneshot::Sender<()>,
        error: Option<String>,
        recover_input: bool,
        rejected: Vec<Input>,
    },
    WindowUnknown,
    Interrupted {
        resend: bool,
        rejected: Vec<Input>,
        ack: tokio::sync::oneshot::Sender<()>,
    },
    Delivering {
        text: String,
        skills: Vec<crate::history::Skill>,
    },
    Messages(Vec<(usize, Arc<str>)>),
    Rewound(Result<(String, Box<Transcript>, crate::agent::Accounting), String>),

    MessageBoundary,
    Review(crate::history::Review),
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
    // The UI can still accept steering until it observes the outcome. Do not let
    // another wake or turn consume that input before the UI applies the boundary.
    async fn boundary(
        &self,
        event: impl FnOnce(tokio::sync::oneshot::Sender<()>) -> UiEvent,
    ) -> Option<()> {
        let (ack, applied) = tokio::sync::oneshot::channel();
        self.send(event(ack)).await;
        applied.await.ok()
    }

    async fn send(&self, event: UiEvent) {
        let _ = self.queue.send(vec![event]).await;
    }
}

struct Reporter<'a> {
    updates: &'a Sender,
    pending: Vec<UiEvent>,
    rejected: Vec<Input>,
    refused: bool,
}
impl<'a> Reporter<'a> {
    fn new(updates: &'a Sender) -> Self {
        Self {
            updates,
            pending: Vec::new(),
            rejected: Vec::new(),
            refused: false,
        }
    }
}
impl crate::agent::Reporter for &mut Reporter<'_> {
    fn event(&mut self, event: AgentEvent<'_>) {
        if matches!(event, AgentEvent::Refused) {
            self.refused = true;
            return;
        }
        if let AgentEvent::InputRejected(input) = event {
            self.rejected.push(input.clone());
            return;
        }
        let Some(event) = ui_event(event) else {
            return;
        };
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
        task: tokio::task::JoinHandle<io::Result<()>>,
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
    pub(super) async fn stop(&mut self) -> io::Result<()> {
        let result = match self {
            Self::Running { task, control } => {
                control.quit();
                task.await
            }
            Self::Stopped => return Ok(()),
        };
        *self = Self::Stopped;
        result.map_err(io::Error::other)?
    }
    pub(super) async fn ended(&mut self) -> io::Error {
        let result = match self {
            Self::Running { task, .. } => task.await,
            Self::Stopped => std::future::pending().await,
        };
        *self = Self::Stopped;
        match result {
            Ok(Ok(())) => io::Error::other("agent session ended unexpectedly"),
            Ok(Err(error)) => error,
            Err(error) => io::Error::other(format!("agent session failed: {error}")),
        }
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
    Turn(u64),
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
) -> io::Result<()> {
    let mut shutdown = updates.control.signal.subscribe();
    let drive = async {
        session.prewarm(&mut Reporter::new(&updates)).await;
        drive_session(&mut session, commands, updates, state).await;
    };
    tokio::select! {
        biased;
        _ = shutdown.wait_for(|signal| matches!(signal, Some(Signal::Quit))) => {}
        () = drive => {}
    }
    crate::finish_session(&mut session)
        .await
        .map_err(io::Error::other)
}

async fn drive_session(
    session: &mut Session,
    mut commands: mpsc::Receiver<Command>,
    updates: Sender,
    state: std::path::PathBuf,
) {
    let mut control = updates.control.signal.subscribe();
    control.mark_changed();
    let inbox: Inbox = session.inbox();
    let mut rename = false;
    let mut named = session.journal().name().is_some();
    loop {
        let work = if std::mem::take(&mut rename) {
            Work::Rename(None)
        } else {
            let Some(work) = idle(session, &mut commands, &updates, &state, &mut control).await
            else {
                break;
            };
            work
        };
        if matches!(*control.borrow(), Some(Signal::Quit)) {
            return;
        }
        let turn = matches!(work, Work::Turn(_));
        let mut report = Reporter::new(&updates);
        let mut rename_failed = false;
        let outcome = {
            let run = async {
                if !turn {
                    report.pending.push(UiEvent::Started(Vec::new()));
                    (&mut report).flush().await;
                }
                Some(match work {
                    Work::Turn(token) => {
                        let inputs = updates.control.claim_input(&inbox, token);
                        // A recalled or superseded prompt can leave a stale command behind.
                        if inputs.is_empty() {
                            return None;
                        }
                        let input = merge(inputs.clone(), "\n\n");
                        report.pending.push(UiEvent::Started(inputs));
                        session.turn(input, &mut report).await
                    }
                    Work::Compact => session.compact(&mut report).await,
                    Work::Wake => session.wake(&mut report).await,
                    Work::Rename(name) => {
                        let result = session.rename(name.as_deref(), &mut report).await;
                        rename_failed = result.is_err();
                        // A failed automatic name leaves the session unnamed without a message.
                        if let (Some(_), Err(error)) = (&name, result) {
                            report.pending.push(UiEvent::Renamed(Err(error)));
                        }
                        Ok(())
                    }
                })
            };
            tokio::select! {
                biased;
                () = async { let _ = control.changed().await; } => match *control.borrow_and_update() {
                    Some(Signal::Interrupt { resend }) => Outcome::Interrupt { resend },
                    Some(Signal::Quit) | None => Outcome::Quit,
                },
                result = run => match result {
                    Some(result) => Outcome::Ended(result.err()),
                    None => continue,
                },
                outcome = interruption(&mut commands, &updates) => outcome,
            }
        };
        if !matches!(outcome, Outcome::Quit) {
            (&mut report).flush().await;
        }
        let applied = match outcome {
            Outcome::Ended(error) => {
                let refused = report.refused;
                let successful =
                    !rename_failed && error.is_none() && !refused && report.rejected.is_empty();
                if successful && turn && !named {
                    named = true;
                    if session.journal().name().is_none() {
                        rename = true;
                    }
                }
                if matches!(error, Some(crate::agent::Error::UnknownWindow(_))) {
                    let () = updates.send(UiEvent::WindowUnknown).await;
                }
                updates
                    .boundary(|ack| UiEvent::Ended {
                        ack,
                        recover_input: !successful,
                        error: error
                            .map(|error| error.to_string())
                            .or_else(|| refused.then(|| "model refused the turn".into())),
                        rejected: std::mem::take(&mut report.rejected),
                    })
                    .await
            }
            Outcome::Interrupt { resend } => {
                updates
                    .boundary(|ack| UiEvent::Interrupted {
                        resend,
                        rejected: report.rejected,
                        ack,
                    })
                    .await
            }
            Outcome::Quit => return,
        };
        if applied.is_none() {
            return;
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
                let signal = *control.borrow_and_update();
                match signal {
                    Some(Signal::Quit) => return None,
                    Some(Signal::Interrupt { resend }) => {
                        updates.boundary(|ack| UiEvent::Interrupted { resend, rejected: Vec::new(), ack }).await?;
                    }
                    None => {}
                }
                continue;
            }
            command = commands.recv() => match command {
                Some(Command::StartTurn(token)) => Work::Turn(token),
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

async fn interruption(commands: &mut mpsc::Receiver<Command>, updates: &Sender) -> Outcome {
    loop {
        match commands.recv().await {
            None => return Outcome::Quit,
            Some(command) => {
                if let Some(event) = refused(command) {
                    let () = updates.send(event).await;
                }
            }
        }
    }
}

fn refused(command: Command) -> Option<UiEvent> {
    Some(match command {
        Command::StartTurn(_) => return None,
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
    })
}

fn ui_event(event: AgentEvent<'_>) -> Option<UiEvent> {
    Some(match event {
        AgentEvent::InputAccepted(input) => UiEvent::Delivering {
            text: input.text.clone(),
            skills: input.skills.clone(),
        },
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
        AgentEvent::Refused | AgentEvent::InputRejected(_) => return None,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupt_before_input_acceptance_only_commits_the_retry() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = crate::tool::test_support::TempDir::new("tui-resend-before-accept");
            let (entered, release) = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            let gates = (entered.clone(), release.clone());
            let mut first = true;
            let (provider, requests) = crate::provider::mock::serve_with(
                dir.path(),
                vec![crate::provider::mock::Reply::text("answer")],
                move |index| {
                    let wait = index.is_none() && std::mem::take(&mut first);
                    let (entered, release) = gates.clone();
                    async move {
                        if wait {
                            entered.notify_one();
                            release.notified().await;
                        }
                    }
                },
            )
            .await;
            let mut session = Session::new_ephemeral(
                reqwest::Client::new(),
                crate::auth::Store::new(dir.path().join("auth.json")),
                Settings {
                    provider,
                    model: "m".into(),
                    reasoning_effort: None,
                },
                Current::default(),
                crate::tool::test_support::context_with(
                    dir.path(),
                    crate::bart_exe(),
                    crate::tool::Confinement::Unrestricted,
                    1000,
                ),
                "fixture".into(),
            );
            let inbox = session.inbox();
            let (sender, mut events) = channel();
            let control = sender.control.clone();
            let (commands, receiver) = mpsc::channel(4);
            let state = dir.path().join("state.json");
            let task = tokio::spawn(async move {
                // Start with a cold route so input is held during the catalog lookup.
                drive_session(&mut session, receiver, sender, state).await;
                session.journal().user_messages()
            });

            assert!(control.enqueue(&inbox, &commands, "unique prompt".to_owned().into(), true));
            entered.notified().await;
            control.interrupt(true);
            let interrupted = loop {
                let batch = events.recv().await.unwrap();
                let mut interrupted = None;
                for event in batch {
                    match event {
                        UiEvent::Delivering { .. } => {
                            panic!("unaccepted input reached the transcript")
                        }
                        UiEvent::Interrupted {
                            resend: true,
                            ack,
                            rejected,
                        } => {
                            assert!(rejected.is_empty());
                            interrupted = Some(ack);
                        }
                        _ => {}
                    }
                }
                if let Some(ack) = interrupted {
                    break ack;
                }
            };
            assert!(requests.lock().unwrap().is_empty());
            assert_eq!(
                control.resume_input(&inbox, &commands),
                Resume::Started,
                "interrupted input must be requeued"
            );
            interrupted.send(()).unwrap();
            release.notify_one();

            let mut delivered = Vec::new();
            let ended = loop {
                let mut ended = None;
                for event in events.recv().await.unwrap() {
                    match event {
                        UiEvent::Delivering { text, .. } => delivered.push(text),
                        UiEvent::Ended { ack, error, .. } => {
                            assert!(error.is_none(), "{error:?}");
                            ended = Some(ack);
                        }
                        _ => {}
                    }
                }
                if let Some(ack) = ended {
                    break ack;
                }
            };
            assert_eq!(delivered, ["unique prompt"]);
            control.quit();
            drop(ended);
            let history = task.await.unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(&*history[0].1, "unique prompt");
            assert_eq!(requests.lock().unwrap().len(), 1);
        })
        .await
        .unwrap();
    }
}
