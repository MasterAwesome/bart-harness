use super::session::Command;
use super::tasks::{Copy, Kind, OpenFrom, Ticket};
use super::viewer::Viewer;
use super::{
    composer::Composer,
    paste,
    picker::{Choice, Picker},
    progress,
    transcript::Transcript,
};
use crate::agent::Inbox;
use crate::tui::MAX_DRAFT_BYTES;
use crate::tui::picker::{self, Purpose};
use crate::tui::status::Kind as StatusKind;
use crate::tui::text::Clean;
use crate::tui::widgets::confirm::{Actions, Confirm, Outcome as ConfirmOutcome};
use crate::tui::{Screen, synchronized};
use crate::tui::{
    input::ReadEvent,
    session::UiEvent,
    tasks::{Done, Task, TaskResult},
};
use crate::{agent::Session, preferences::Change, provider::Provider};
use crate::{
    agent::{INPUT_FULL, Input, InputSize, Settings, inputs_fit},
    history::InlineImage,
    provider::Model,
    tool::Confinement,
};
use crossterm::event::{Event, KeyEventKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use keys::parse_slash;
use ratatui::text::Text;
use std::sync::Arc;
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_COMPOSER_ROWS: usize = 6;
const QUIT_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    Send,
    Queue,
}

pub(super) struct PreparedInput {
    pub(super) text: String,
    pub(super) skills: Vec<crate::history::Skill>,
}

pub(super) struct Preparation {
    pub(super) previews: super::paste::Previews,
    pub(super) draft: String,
    pub(super) delivery: Delivery,
    pub(super) composer: Composer,
    pub(super) result: Result<PreparedInput, String>,
}

struct Approval {
    answer: crate::approval::Answer,
    confirm: Confirm,
}

pub(super) struct App {
    shared: super::tasks::Shared,
    session: Option<SessionView>,
    info: SessionInfo,
    turn: Option<Turn>,
    drafts: Drafts,
    transcript: Transcript,
    scrollback: super::scrollback::Scrollback,
    surface: Option<Surface>,
    status: super::status::Status,
    paint: Paint,
    geometry: Geometry,
    work: Work,
    effects: Vec<Effect>,
    first_run: bool,
    quit_armed: Option<Instant>,
}
struct SessionInfo {
    settings: Settings,
    preferences: crate::preferences::Current,
    accounting: crate::agent::Accounting,
    limits: Option<crate::provider::Limits>,
    name: Option<Clean>,
    recording_stopped: bool,
    confinement: Confinement,
    mcp_retry: Option<Change>,
}
struct Turn {
    start_input: Option<u64>,
    progress: progress::Progress,
    started: Instant,
    animation: Animation,
}
impl Turn {
    fn new() -> Self {
        Self {
            start_input: None,
            progress: progress::Progress::default(),
            started: Instant::now(),
            animation: Animation::default(),
        }
    }
}
struct Animation {
    tick: usize,
    started: Instant,
    visible: bool,
}
impl Default for Animation {
    fn default() -> Self {
        Self {
            tick: 0,
            started: Instant::now(),
            visible: false,
        }
    }
}
pub(super) struct PendingInput {
    pub(super) input: Input,
    display: Clean,
    order: u64,
}
impl PendingInput {
    fn new(input: Input, order: u64) -> Self {
        let display = Clean::new(&input.text);
        Self {
            input,
            display,
            order,
        }
    }
}
struct Drafts {
    composer: Composer,
    queued: Vec<PendingInput>,
    steers: Vec<PendingInput>,
    next_pending: u64,
    images: Vec<InlineImage>,
    next_image: usize,
    sent: Vec<String>,
    browsing: Option<(usize, Composer)>,
    pastes: Option<crate::tool::Artifacts>,
    next_paste: usize,
    menu: usize,
    files: Option<super::mention::Index>,
    mention_closed: bool,
}
impl Drafts {
    fn pending(&mut self, input: Input) -> PendingInput {
        let order = self.next_pending;
        self.next_pending += 1;
        PendingInput::new(input, order)
    }

    fn has_pending(&self) -> bool {
        !self.queued.is_empty() || !self.steers.is_empty()
    }

    fn restored_composer(&self, text: &str) -> io::Result<Composer> {
        if let Some(artifacts) = &self.pastes {
            Composer::restore(text, artifacts)
        } else {
            let mut composer = Composer::default();
            if composer.set(text.to_owned()) {
                Ok(composer)
            } else {
                Err(io::Error::other(
                    "draft limit is 1 MiB; draft replacement refused",
                ))
            }
        }
    }
}

impl Default for Drafts {
    fn default() -> Self {
        Self {
            composer: Composer::default(),
            queued: Vec::new(),
            steers: Vec::new(),
            next_pending: 0,
            images: Vec::new(),
            next_image: 1,
            sent: Vec::new(),
            browsing: None,
            pastes: None,
            next_paste: 1,
            menu: 0,
            files: None,
            mention_closed: false,
        }
    }
}
#[derive(Default)]
struct Paint {
    last: Option<view::Painted>,
    seq: u64,
    idle_animation: Animation,
}
struct Geometry {
    size: ratatui::layout::Size,
    resized_at: Option<Instant>,
    conversation: ratatui::layout::Rect,
}
#[derive(Default)]
struct Work {
    pending: std::collections::HashMap<Kind, Ticket>,
    next_ticket: u64,
    preferences: Option<(Ticket, Change)>,
}

impl Work {
    fn ticket(&mut self, kind: Kind) -> Ticket {
        let ticket = Ticket {
            kind,
            seq: self.next_ticket,
        };
        self.next_ticket += 1;
        ticket
    }
    fn take_current(&mut self, ticket: Ticket) -> Option<Ticket> {
        if self
            .pending
            .get(&ticket.kind)
            .is_some_and(|pending| *pending == ticket)
        {
            self.pending.remove(&ticket.kind)
        } else {
            None
        }
    }
}
impl Paint {
    fn record(&mut self, painted: Option<view::Painted>) {
        if let Some(painted) = &painted {
            self.seq = painted.stamp.seq;
        }
        self.last = painted;
    }
}
impl Animation {
    fn advance(&mut self) {
        self.tick = (self.started.elapsed().as_millis() / 100) as usize;
    }
}

#[derive(Clone, Copy)]
pub(super) enum CatalogUse {
    Picker,
    Effort,
    Step { up: bool },
    Settings,
}

pub(super) fn merge(inputs: Vec<Input>, separator: &str) -> Input {
    let mut merged = Input::default();
    let mut texts = Vec::new();
    for input in inputs {
        texts.push(input.text);
        merged.images.extend(input.images);
        merged.pasted_bytes += input.pasted_bytes;
        merged.skills.extend(input.skills);
    }
    merged.text = texts.join(separator);
    merged
}

mod events;
mod input;
mod keys;
mod paint;
mod results;
mod view;

pub(super) struct SessionView {
    id: String,
    inbox: Inbox,
    live: Arc<crate::tool::Live>,
    context: Arc<crate::tool::Context>,
    jobs: Arc<crate::tool::Jobs>,
}

enum Surface {
    Picker(Picker),
    Viewer(Box<Viewer>, Option<Picker>),
    Approval(Approval),
}
impl SessionView {
    pub(super) fn inbox(&self) -> &Inbox {
        &self.inbox
    }
    pub(super) fn id(&self) -> &str {
        &self.id
    }
    pub(super) fn live(&self) -> &Arc<crate::tool::Live> {
        &self.live
    }
    pub(super) fn registry(&self) -> Arc<crate::mcp::Registry> {
        self.context.mcp()
    }
    pub(super) fn running_jobs(&self) -> usize {
        self.jobs.running_count()
    }
}

pub(in crate::tui) enum Msg {
    Input(ReadEvent),
    Events(Vec<UiEvent>),
    Done(Done),
    Timer,
    PrepareFrame,
    Painted(Option<view::Painted>),
    Wake,
    RegistryChanged,
    Size(ratatui::layout::Size),
    Clipboard(io::Result<String>),
    Attached(Box<crate::tui::tasks::Opened>),
    Begin,
    CommandRefused(Command),
    InputRecovered(Vec<Input>),
    AdvanceQueue,
    SteerRecalled(Option<Composer>),
    Steering {
        pending: PendingInput,
        accepted: bool,
        start: bool,
    },
}
pub(in crate::tui) enum InputBoundary {
    Advance,
    Resend,
    Recover(Vec<Input>),
}

pub(in crate::tui) enum Effect {
    Send(Command),
    Interrupt {
        resend: bool,
    },
    Approve {
        answer: crate::approval::Answer,
        allow: bool,
    },
    Enqueue {
        pending: PendingInput,
        start: bool,
    },
    RecallSteer {
        input: Input,
        composer: Composer,
    },
    InputBoundary {
        next: InputBoundary,
        ack: tokio::sync::oneshot::Sender<()>,
    },
    Spawn(Ticket, Task),
    Cancel(Kind),
    CopyToTerminal {
        text: String,
        what: String,
    },
    Clear,
    Suspend,
    Reassert,
    RebindRegistry,
    Quit,
    Exec(crate::relaunch::Plan),
    Open {
        next: Box<(Session, String)>,
        close: Ticket,
        delete_previous: Option<String>,
    },
}

impl App {
    pub(super) fn new(
        settings: Settings,
        preferences: crate::preferences::Current,
        shared: super::tasks::Shared,
        size: ratatui::layout::Size,
        first_run: bool,
    ) -> Self {
        Self {
            shared,
            session: None,
            info: SessionInfo {
                settings,
                preferences,
                accounting: crate::agent::Accounting::default(),
                limits: None,
                name: None,
                recording_stopped: false,
                confinement: Confinement::Unrestricted,
                mcp_retry: None,
            },
            turn: None,
            drafts: Drafts::default(),
            transcript: Transcript::default(),
            scrollback: super::scrollback::Scrollback::default(),
            surface: None,
            status: super::status::Status::default(),
            paint: Paint::default(),
            geometry: Geometry {
                size,
                resized_at: None,
                conversation: ratatui::layout::Rect::default(),
            },
            work: Work::default(),
            effects: Vec::new(),
            first_run,
            quit_armed: None,
        }
    }

    pub(in crate::tui) fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Input(ReadEvent::Terminal(event)) => self.input(event),
            Msg::Input(ReadEvent::Refused(reason)) => self.set_status(StatusKind::Info, reason),
            Msg::Input(ReadEvent::Resized) => self.geometry.resized_at = Some(Instant::now()),
            Msg::Input(ReadEvent::Terminate) => self.effects.push(Effect::Quit),
            Msg::Input(ReadEvent::Suspend) => self.effects.push(Effect::Suspend),
            Msg::Input(ReadEvent::Continued) => self.effects.push(Effect::Reassert),
            Msg::Events(batch) => {
                for event in batch {
                    let text = self.agent_event(event, self.geometry.size.width);
                    self.append_display(text);
                }
            }
            Msg::Done(done) => self.task_done(done),
            Msg::Timer => self.status.expire(),
            Msg::PrepareFrame => self.prepare_frame(),
            Msg::Painted(painted) => self.painted(painted),
            Msg::Wake => {}
            Msg::RegistryChanged => {
                if let Some(Surface::Picker(picker)) = &mut self.surface {
                    picker.registry_changed();
                }
            }
            Msg::InputRecovered(inputs) => self.restore_steers(inputs),
            Msg::AdvanceQueue => {
                if !self.drafts.queued.is_empty() {
                    let next = self.drafts.queued.remove(0);
                    self.submit_input(next);
                }
            }
            Msg::SteerRecalled(composer) => {
                if let Some(composer) = composer {
                    self.restore_pending(true, composer);
                } else {
                    self.drafts.steers.pop();
                    self.set_status(StatusKind::Info, "steering already received by the agent");
                }
            }
            Msg::Steering {
                pending,
                start,
                accepted,
            } => {
                if accepted {
                    if start && let Some(turn) = &mut self.turn {
                        turn.start_input = Some(pending.order);
                    }
                    self.drafts.steers.push(pending);
                } else {
                    if start {
                        self.turn = None;
                    }
                    if self.restore_draft(&pending.input.text) {
                        self.drafts.images = pending.input.images;
                    } else {
                        self.drafts.queued.insert(0, pending);
                    }
                    self.set_status(StatusKind::Info, INPUT_FULL);
                }
            }
            Msg::Clipboard(result) => match result {
                Ok(text) => self.set_status(StatusKind::Info, text),
                Err(error) => self.set_status(StatusKind::Error, error.to_string()),
            },
            Msg::Size(size) => self.resized(size),
            Msg::CommandRefused(command) => {
                match command {
                    Command::Compact | Command::Rename(_) => self.turn = None,
                    Command::Prefer(..) => {
                        self.work.preferences = None;
                        if let Some(Surface::Picker(picker)) = &mut self.surface {
                            picker.preference_failed();
                        }
                    }
                    _ => {}
                }
                self.set_status(
                    StatusKind::Info,
                    "agent command queue is full or closed; command refused",
                );
            }
            Msg::Attached(opened) => self.attach_session(*opened),
            Msg::Begin => {
                self.set_picker(Some(Picker::providers(Vec::new(), false)));
                self.load_providers(None);
            }
        }
        std::mem::take(&mut self.effects)
    }

    fn needs_context(&self) -> bool {
        matches!(&self.surface, Some(Surface::Picker(picker)) if picker.needs_context())
            && !self.work.pending.contains_key(&Kind::Catalog)
    }

    fn cancel_picker_tasks(&mut self) {
        for kind in [Kind::Catalog, Kind::Providers, Kind::Sessions] {
            self.cancel_task(kind);
        }
    }

    fn set_picker(&mut self, picker: Option<Picker>) {
        self.status.clear();
        self.cancel_picker_tasks();
        self.cancel_task(Kind::Image);
        self.cancel_task(Kind::Prepare);
        self.surface = picker.map(Surface::Picker);
    }

    fn append_display(&mut self, texts: impl IntoIterator<Item = super::transcript::Block>) {
        let removed = self.transcript.append(texts, self.scrollback.protected());
        self.scrollback.evicted(&removed);
    }

    fn set_status(&mut self, kind: StatusKind, text: impl AsRef<str>) {
        let text = Clean::new(text.as_ref());
        if kind != StatusKind::Info {
            self.transcript.ui_notice(kind, &text);
        }
        let persistent = kind != StatusKind::Info
            && matches!(&self.surface, Some(Surface::Picker(picker)) if !picker.confirmation_active());
        let duration = (!persistent).then(|| {
            Duration::from_secs(if matches!(self.surface, Some(Surface::Picker(_))) {
                4
            } else {
                3
            })
        });
        self.status.set(kind, text, duration);
    }

    fn clear_picker_feedback(&mut self) {
        if matches!(self.surface, Some(Surface::Picker(_))) || self.status.persistent() {
            self.status.clear();
        }
    }

    fn set_approval(&mut self, answer: crate::approval::Answer) {
        self.cancel_task(Kind::Image);
        self.status.clear();
        let text = Clean::new(answer.prompt());
        self.cancel_picker_tasks();
        let mut confirm = Confirm::new(&text, Actions::Approve);
        if let Some(code) = answer.code() {
            let prompt = &text;
            let mut lines: Vec<_> = prompt[..code.start]
                .lines()
                .map(|l| ratatui::text::Line::from(l.to_owned()))
                .collect();
            lines.extend(crate::tui::highlight::python(
                &crate::tui::highlight::statements(&prompt[code.clone()]),
            ));
            lines.extend(
                prompt[code.end..]
                    .trim_start_matches('\n')
                    .lines()
                    .map(|l| ratatui::text::Line::from(l.to_owned())),
            );
            confirm.review = lines.into();
        }
        confirm.title = text
            .lines()
            .next()
            .unwrap_or_default()
            .trim_end_matches(':')
            .into();
        if !confirm.review.lines.is_empty() {
            confirm.review.lines.remove(0);
        }
        self.surface = Some(Surface::Approval(Approval { answer, confirm }));
    }

    fn copy_text(&mut self, text: String, what: String) {
        self.start_task(Task::Copy(Copy::Text { text, what }));
    }

    fn start_task(&mut self, task: Task) {
        let ticket = self.work.ticket(task.kind());
        self.work.pending.insert(ticket.kind, ticket);
        self.effects.push(Effect::Spawn(ticket, task));
    }

    fn cancel_task(&mut self, kind: Kind) {
        if self.work.pending.remove(&kind).is_some() {
            self.effects.push(Effect::Cancel(kind));
        }
    }

    fn active_task(&self, kind: Kind) -> bool {
        self.work.pending.contains_key(&kind)
    }

    fn send_command(&mut self, command: Command) {
        self.effects.push(Effect::Send(command));
    }

    fn close_details(&mut self) {
        if let Some(Surface::Viewer(_, picker)) = self.surface.take() {
            self.surface = picker.map(Surface::Picker);
        }
    }

    fn open_details(&mut self, error: Option<String>) {
        let latest = error.is_some();
        if let Some(error) = error {
            self.transcript.note("picker details", &error);
        }
        let Some(session) = &self.session else {
            return;
        };
        let viewer = Viewer::new(self.transcript.details(), &session.live, latest);
        self.cancel_picker_tasks();
        self.cancel_task(Kind::Image);
        self.cancel_task(Kind::Prepare);
        let picker = match self.surface.take() {
            Some(Surface::Picker(picker)) => Some(picker),
            _ => None,
        };
        self.surface = Some(Surface::Viewer(Box::new(viewer), picker));
    }

    pub(in crate::tui) fn session_view(&self) -> Option<&SessionView> {
        self.session.as_ref()
    }

    pub(in crate::tui) fn title(&self) -> String {
        self.info
            .name
            .as_deref()
            .unwrap_or(&self.shared.directory)
            .replace(['\n', '\t'], " ")
    }

    pub(in crate::tui) fn state_path(&self) -> PathBuf {
        self.shared.state.clone()
    }

    pub(super) fn sessions(&self) -> crate::record::Sessions {
        self.shared.sessions.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        history::test_support::{PNG, PNG_BASE64},
        preferences::Current,
        provider::mock::Reply,
        tui::{
            Runtime,
            session::Command,
            tasks::{self, CatalogLoader, Shared},
        },
    };
    use ratatui::layout::Size;

    fn run_runtime_effects(app: &mut App, runtime: &mut Runtime, initial: Vec<Effect>) {
        let mut effects: std::collections::VecDeque<_> = initial.into();
        while runtime.next_effect(app, &mut effects).is_some() {}
    }

    #[tokio::test]
    async fn copy_command_uses_raw_answers_and_ignores_stale_clipboard_results() {
        let dir = tempfile::tempdir().unwrap();
        let (provider, _) = crate::provider::mock::serve(dir.path(), vec![]).await;
        let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;
        let submit = |app: &mut App, command: &str| {
            app.insert(command);
            app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))))
        };
        let copy = |app: &mut App, command, expected_text, expected_what| {
            let effects = submit(app, command);
            assert!(
                matches!(effects.as_slice(), [Effect::Spawn(ticket, Task::Copy(
                Copy::Text { text, what }
            ))] if ticket.kind == Kind::Copy && text == expected_text && what == expected_what)
            );
            let Effect::Spawn(ticket, _) = effects[0] else {
                unreachable!();
            };
            ticket
        };
        let status = |app: &mut App, command, expected| {
            assert!(submit(app, command).is_empty());
            assert_eq!(app.status.text(), Some(expected));
        };
        status(&mut app, "/copy", "no answer yet");
        for invalid in ["/copy -1", "/copy +1", "/copy word", "/copy 0 1"] {
            status(&mut app, invalid, "usage: /copy [N] (0 = latest answer)");
        }
        let first = "\n    code\n";
        let second = "**answer**  \n";
        app.update(Msg::Events(vec![
            UiEvent::Text(first.into()),
            UiEvent::MessageBoundary,
            UiEvent::Responded,
            UiEvent::Text(" \n\t".into()),
            UiEvent::ToolStarted {
                id: "call".into(),
                name: "fixture".into(),
                arguments: serde_json::json!({}).into(),
                live: None,
            },
            UiEvent::Responded,
        ]));
        status(&mut app, "/copy 1", "only 1 answer available (use /copy 0)");
        let stale = copy(&mut app, "/copy", first, "the last answer");
        app.turn = Some(Turn::new());
        app.update(Msg::Events(vec![
            UiEvent::Text("**answer".into()),
            UiEvent::Text("**  \n".into()),
        ]));
        copy(&mut app, "/copy  0  ", second, "the last answer");
        let current = copy(&mut app, "/copy 1", first, "the previous answer");
        status(
            &mut app,
            "/copy 2",
            "only 2 answers available (use /copy 0..=1)",
        );
        let result = |ticket| {
            Msg::Done(Done {
                ticket,
                result: TaskResult::CopyToTerminal {
                    text: first.into(),
                    what: "the answer".into(),
                },
            })
        };
        assert!(app.update(result(stale)).is_empty());
        assert!(matches!(app.update(result(current)).as_slice(),
            [Effect::CopyToTerminal { text, .. }] if text == first));
        for code in ['c', 'C'] {
            let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Char(code),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )))));
            assert!(
                matches!(effects.as_slice(), [Effect::Spawn(ticket, Task::Copy(Copy::Conversation(_)))] if ticket.kind == Kind::Copy)
            );
        }
        let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Spawn(_, Task::Copy(_))))
        );
        assert!(app.quit_armed.is_some());
        runtime.running.take().unwrap().driver.stop().await.unwrap();
        assert_eq!(
            runtime.tasks.finish(app.sessions()).await,
            [] as [String; 0]
        );
    }

    fn transcript_copies(app: &App, text: &str) -> usize {
        app.transcript
            .blocks()
            .iter()
            .flat_map(|entry| entry.block.rows.iter())
            .flat_map(|row| row.project(80))
            .filter(|line| line.to_string().contains(text))
            .count()
    }

    #[tokio::test]
    async fn unknown_window_restores_draft_and_attachment_without_sending_twice() {
        let dir = crate::tool::test_support::TempDir::new("tui-unknown-window");
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let gates = (entered.clone(), release.clone());
        let mut prewarming = true;
        let (provider, requests) = crate::provider::mock::serve_with(
            dir.path(),
            vec![Reply::text("seen")],
            move |index| {
                let wait = index.is_some() || std::mem::take(&mut prewarming);
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
        let settings = Settings {
            provider,
            model: "fixture".into(),
            reasoning_effort: None,
        };
        let store = crate::auth::Store::new(dir.path().join("auth.json"));
        let config = dir.path().join("config");
        let state = dir.path().join("state.json");
        let sessions = crate::record::Sessions::new(&dir.path().join("sessions"), dir.path());
        let client = reqwest::Client::new();
        let opener = crate::agent::Opener {
            sessions: sessions.clone(),
            cwd: dir.path().to_path_buf(),
            executable: crate::bart_exe(),
            client: client.clone(),
            store: store.clone(),
        };
        let shared = Shared {
            loader: CatalogLoader {
                client: client.clone(),
                store: store.clone(),
                config,
            },
            opener,
            sessions,
            state,
            directory: Clean::new("test"),
        };

        let provider_name = settings.provider.name().to_owned();
        let session = Session::new_ephemeral(
            client,
            store,
            settings.clone(),
            Current::default(),
            crate::tool::test_support::context_with(
                dir.path(),
                crate::bart_exe(),
                Confinement::Unrestricted,
                1000,
            ),
            "fixture".into(),
        );
        let inbox = session.inbox();
        let mut app = App::new(
            settings,
            Current::default(),
            shared,
            Size::new(80, 24),
            false,
        );
        let mut runtime = Runtime::new(app.sessions());
        let attached = app.update(Msg::Attached(tasks::opened((session, "s1".into())).await));
        run_runtime_effects(&mut app, &mut runtime, attached);

        entered.notified().await;
        let image = InlineImage::new(PNG.to_vec(), None, "[image #1]".into()).unwrap();
        assert!(app.drafts.composer.insert("inspect this [image #1]"));
        app.drafts.images.push(image.clone());
        app.prepare_or_commit(Delivery::Send);
        assert!(app.drafts.composer.is_empty());
        assert!(app.drafts.images.is_empty());
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);

        // Up can recall the first send while the driver is still prewarming.
        let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))));
        run_runtime_effects(&mut app, &mut runtime, effects);
        assert_eq!(app.drafts.composer.text(), "inspect this [image #1]");
        assert_eq!(app.drafts.images.as_slice(), std::slice::from_ref(&image));
        assert!(inbox.is_empty());
        assert!(app.turn.is_none(), "nothing is left to start or end a turn");
        release.notify_one();
        run_runtime_effects(
            &mut app,
            &mut runtime,
            vec![Effect::Send(Command::Messages)],
        );
        let batch = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
            .await
            .expect("driver did not publish prewarm accounting")
            .expect("driver exited early");
        assert!(
            matches!(batch.as_slice(), [UiEvent::Accounting(accounting)] if accounting.context_budget.is_none())
        );
        let effects = app.update(Msg::Events(batch));
        run_runtime_effects(&mut app, &mut runtime, effects);
        let batch = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
            .await
            .expect("driver did not reach idle after recall")
            .expect("driver exited early");
        assert!(matches!(batch.as_slice(), [UiEvent::Messages(messages)] if messages.is_empty()));
        assert!(
            requests.lock().unwrap().is_empty(),
            "recall must not auto-name an empty session"
        );

        app.input(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(app.drafts.queued.is_empty());
        app.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);

        let mut saw_window_unknown = false;
        let mut deferred = Vec::new();
        let mut saw_ended_err = false;
        while !saw_ended_err {
            let batch = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
                .await
                .expect("timed out waiting for UnknownWindow")
                .expect("driver exited early");
            for event in &batch {
                if matches!(event, UiEvent::WindowUnknown) {
                    saw_window_unknown = true;
                }
                if matches!(event, UiEvent::Ended { error: Some(_), .. }) {
                    saw_ended_err = true;
                }
            }
            deferred.extend(batch);
        }
        app.insert("late on failed turn");
        app.prepare_or_commit(Delivery::Send);
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);
        let effects = app.update(Msg::Events(deferred));
        run_runtime_effects(&mut app, &mut runtime, effects);
        assert!(saw_window_unknown);
        assert_eq!(transcript_copies(&app, "inspect this [image #1]"), 0);
        assert!(
            app.turn.is_none(),
            "a rejected idle send must leave the UI idle"
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(
            inbox.take().is_empty(),
            "rejected input must not stay queued in inbox"
        );
        assert_eq!(
            app.drafts.composer.text(),
            "inspect this [image #1]\nlate on failed turn"
        );
        assert_eq!(app.drafts.images, vec![image]);

        app.surface = None;
        let ticket = Ticket {
            kind: Kind::Preferences,
            seq: 1,
        };
        run_runtime_effects(
            &mut app,
            &mut runtime,
            vec![Effect::Send(Command::Prefer(
                Change::ContextWindow {
                    provider: provider_name,
                    model: "fixture".into(),
                    tokens: Some(crate::preferences::ContextWindow::try_from(100_000).unwrap()),
                },
                ticket,
            ))],
        );
        loop {
            let batch = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
                .await
                .expect("timed out waiting for Preferred")
                .expect("driver exited");
            let mut saved = false;
            for event in &batch {
                if let UiEvent::Preferred(got, result) = event
                    && *got == ticket
                {
                    result.as_ref().expect("saving context window failed");
                    saved = true;
                }
            }
            let effects = app.update(Msg::Events(batch));
            run_runtime_effects(&mut app, &mut runtime, effects);
            if saved {
                break;
            }
        }
        assert_eq!(
            app.info
                .accounting
                .context_budget
                .map(|budget| (budget.window(), budget.compaction_threshold())),
            Some((100_000, 75_000))
        );

        app.prepare_or_commit(Delivery::Send);
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);
        entered.notified().await;
        let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))));
        run_runtime_effects(&mut app, &mut runtime, effects);
        assert!(
            app.turn.is_some(),
            "recall after claim cannot cancel real work"
        );
        assert!(app.drafts.composer.is_empty());
        app.insert("second unclaimed steer");
        app.prepare_or_commit(Delivery::Send);
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);
        let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))));
        run_runtime_effects(&mut app, &mut runtime, effects);
        assert_eq!(app.drafts.composer.text(), "second unclaimed steer");
        assert!(
            app.turn.is_some(),
            "withdrawing a steer cannot clear the claimed turn"
        );
        release.notify_one();
        loop {
            let batch = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
                .await
                .expect("timed out waiting for resent turn")
                .expect("driver exited");
            let done = batch.iter().any(|event| match event {
                UiEvent::Ended { error, .. } => {
                    assert!(error.is_none(), "resend failed: {error:?}");
                    true
                }
                _ => false,
            });
            let effects = app.update(Msg::Events(batch));
            run_runtime_effects(&mut app, &mut runtime, effects);
            if done {
                break;
            }
        }
        if let Some(mut active) = runtime.running.take() {
            active.driver.stop().await.unwrap();
        }
        assert_eq!(
            runtime.tasks.finish(app.sessions()).await,
            Vec::<String>::new()
        );

        assert_eq!(transcript_copies(&app, "inspect this [image #1]"), 1);
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 1, "restored draft must be sent exactly once");
        let serialized = serde_json::to_string(&sent[0]).unwrap();
        assert!(
            serialized.contains("inspect this [image #1]"),
            "{serialized}"
        );
        assert!(serialized.contains(PNG_BASE64), "{serialized}");
    }

    #[tokio::test]
    async fn boundary_ack_waits_for_the_queued_prompt_to_be_enqueued() {
        let dir = crate::tool::test_support::TempDir::new("tui-boundary-ack");
        let (provider, _) =
            crate::provider::mock::serve(dir.path(), vec![Reply::text("unused")]).await;
        let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;
        app.update(Msg::Events(vec![UiEvent::Started(Vec::new())]));
        app.insert("queued prompt");
        app.prepare_or_commit(Delivery::Queue);

        let (ack, mut applied) = tokio::sync::oneshot::channel();
        let mut effects: std::collections::VecDeque<_> = app
            .update(Msg::Events(vec![UiEvent::Ended {
                ack,
                error: None,
                recover_input: false,
                rejected: Vec::new(),
            }]))
            .into();
        assert!(matches!(
            applied.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let boundary = effects.pop_front().unwrap();
        runtime.apply(&mut app, boundary, &mut effects);
        assert!(app.session_view().unwrap().inbox().is_empty());
        assert!(matches!(
            applied.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(runtime.next_effect(&mut app, &mut effects).is_none());
        applied.try_recv().unwrap();
        assert_eq!(
            app.session_view().unwrap().inbox().pop().unwrap().text,
            "queued prompt"
        );
        assert!(app.drafts.queued.is_empty());
        runtime.running.take().unwrap().driver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn interrupt_without_resend_clears_steer_pills_even_when_nothing_is_recovered() {
        let dir = crate::tool::test_support::TempDir::new("tui-interrupt-steer");
        let (provider, _) =
            crate::provider::mock::serve(dir.path(), vec![Reply::text("unused")]).await;
        let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;
        app.update(Msg::Events(vec![UiEvent::Started(Vec::new())]));
        app.insert("steered prompt");
        app.prepare_or_commit(Delivery::Send);
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);
        assert_eq!(app.drafts.steers.len(), 1);
        assert_eq!(
            app.session_view().unwrap().inbox().pop().unwrap().text,
            "steered prompt"
        );

        let (ack, applied) = tokio::sync::oneshot::channel();
        let effects = app.update(Msg::Events(vec![UiEvent::Interrupted {
            resend: false,
            rejected: Vec::new(),
            ack,
        }]));
        run_runtime_effects(&mut app, &mut runtime, effects);
        applied.await.unwrap();
        assert!(app.drafts.steers.is_empty());
        assert!(app.drafts.composer.is_empty());
        assert!(app.turn.is_none());
        runtime.running.take().unwrap().driver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn queued_message_during_auto_rename_is_delivered_and_answered() {
        for interrupt in [None, Some(false), Some(true)] {
            tokio::time::timeout(
                Duration::from_secs(5),
                rename_case(interrupt, NameReply::Valid),
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn steer_after_model_refusal_waits_for_an_explicit_resubmit() {
        let dir = crate::tool::test_support::TempDir::new("late-steer");
        let mut refusal = Reply::text("");
        refusal.chunks[0]["choices"][0]["delta"] = serde_json::json!({"refusal":"No"});
        let (provider, requests) =
            crate::provider::mock::serve(dir.path(), vec![refusal, Reply::text("accepted")]).await;
        let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;
        let enter = |app: &mut App, runtime: &mut Runtime| {
            app.input(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )));
            let effects = std::mem::take(&mut app.effects);
            run_runtime_effects(app, runtime, effects);
        };
        app.insert("first");
        enter(&mut app, &mut runtime);
        let image = InlineImage::new(PNG.to_vec(), None, "[image #1]".into()).unwrap();
        loop {
            let events = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
                .await
                .unwrap()
                .unwrap();
            let refused = events.iter().any(|e| matches!(e, UiEvent::Ended { error: Some(reason), .. } if reason.contains("refused")));
            if refused {
                assert!(app.turn.is_some());
                app.insert("late [image #1]");
                app.drafts.images.push(image.clone());
                enter(&mut app, &mut runtime);
                assert!(!app.session_view().unwrap().inbox().is_empty());
            }
            let effects = app.update(Msg::Events(events));
            run_runtime_effects(&mut app, &mut runtime, effects);
            if refused {
                break;
            }
        }
        assert_eq!(app.drafts.composer.text(), "late [image #1]");
        assert_eq!(app.drafts.images, [image]);
        assert!(app.session_view().unwrap().inbox().is_empty());
        assert_eq!(requests.lock().unwrap().len(), 1);
        enter(&mut app, &mut runtime);
        loop {
            let events = tokio::time::timeout(Duration::from_secs(5), runtime.events.recv())
                .await
                .unwrap()
                .unwrap();
            if events
                .iter()
                .any(|e| matches!(e, UiEvent::Text(t) if t == "accepted"))
            {
                break;
            }
            let effects = app.update(Msg::Events(events));
            run_runtime_effects(&mut app, &mut runtime, effects);
        }
        runtime.running.take().unwrap().driver.stop().await.unwrap();
        assert!(runtime.tasks.finish(app.sessions()).await.is_empty());
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 2);
        let body = sent[1].to_string();
        assert_eq!(body.matches("late [image #1]").count(), 1);
        assert!(body.contains(PNG_BASE64));
    }

    #[tokio::test]
    async fn refused_rename_returns_pending_input_without_starting_a_turn() {
        tokio::time::timeout(
            Duration::from_secs(5),
            rename_case(None, NameReply::Refusal),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tab_submissions_remain_separate_turns_across_auto_rename() {
        tab_turns(false).await;
    }

    #[tokio::test]
    async fn recall_during_auto_rename_restores_the_newer_tab_submission() {
        tab_turns(true).await;
    }

    async fn tab_turns(recall: bool) {
        let dir = crate::tool::test_support::TempDir::new("tab-turns");
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let (provider, requests) = crate::provider::mock::serve_with(
            dir.path(),
            ["first answer", r#"{"name":"auto"}"#, "answer A", "answer B"]
                .map(Reply::text)
                .into(),
            move |index| {
                let gate = gate.clone();
                async move {
                    if recall && index == Some(1) {
                        gate.notified().await;
                    }
                }
            },
        )
        .await;
        let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;
        app.insert("first");
        app.prepare_or_commit(Delivery::Send);
        for (text, label) in [
            ("queued A [image #1]", "[image #1]"),
            ("queued B [image #2]", "[image #2]"),
        ] {
            app.insert(text);
            app.drafts
                .images
                .push(InlineImage::new(PNG.to_vec(), None, label.into()).unwrap());
            app.input(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        }
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut started = 0;
            loop {
                let batch = runtime.events.recv().await.unwrap();
                started += batch.iter().filter(|e| matches!(e, UiEvent::Started(_))).count();
                if batch
                    .iter()
                    .any(|e| matches!(e, UiEvent::Text(t) if t == "answer A"))
                {
                    let sent = requests.lock().unwrap();
                    assert!(
                        !sent[2].to_string().contains("queued B"),
                        "B arrived before A was answered"
                    );
                }
                let done = batch
                    .iter()
                    .any(|e| matches!(e, UiEvent::Text(t) if t == if recall { "answer A" } else { "answer B" }));
                let effects = app.update(Msg::Events(batch));
                run_runtime_effects(&mut app, &mut runtime, effects);
                if recall && started == 2 && app.drafts.composer.is_empty() {
                    app.input(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
                    assert_eq!(app.drafts.composer.text(), "queued B [image #2]");
                    assert_eq!(app.drafts.images, [InlineImage::new(PNG.to_vec(), None, "[image #2]".into()).unwrap()]);
                    release.notify_one();
                }
                if done { break; }
            }
        })
        .await
        .unwrap();
        runtime.running.take().unwrap().driver.stop().await.unwrap();
        assert!(runtime.tasks.finish(app.sessions()).await.is_empty());
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), if recall { 3 } else { 4 });
        for (request, text) in sent[2..]
            .iter()
            .zip(["queued A [image #1]", "queued B [image #2]"])
        {
            let messages = request["messages"].as_array().unwrap();
            let new_turn = &messages[messages.len() - 2..];
            assert!(new_turn[0].to_string().contains(text));
            assert_eq!(new_turn[1].to_string().matches(PNG_BASE64).count(), 1);
        }
    }

    pub(super) async fn attached_fixture(
        dir: &std::path::Path,
        provider: Provider,
    ) -> (App, Runtime) {
        let mut preferences = Current::default();
        preferences
            .apply(&Change::ContextWindow {
                provider: provider.name().to_owned(),
                model: "fixture".into(),
                tokens: Some(crate::preferences::ContextWindow::try_from(100_000).unwrap()),
            })
            .unwrap();
        let settings = Settings {
            provider,
            model: "fixture".into(),
            reasoning_effort: None,
        };
        let store = crate::auth::Store::new(dir.join("auth.json"));
        let config = dir.join("config");
        let state = dir.join("state.json");
        let sessions = crate::record::Sessions::new(&dir.join("sessions"), dir);
        let client = reqwest::Client::new();
        let opener = crate::agent::Opener {
            sessions: sessions.clone(),
            cwd: dir.to_path_buf(),
            executable: crate::bart_exe(),
            client: client.clone(),
            store: store.clone(),
        };
        let shared = Shared {
            loader: CatalogLoader {
                client: client.clone(),
                store: store.clone(),
                config,
            },
            opener,
            sessions,
            state,
            directory: Clean::new("test"),
        };
        let session = Session::new_ephemeral(
            client,
            store,
            settings.clone(),
            preferences.clone(),
            crate::tool::test_support::context_with(
                dir,
                crate::bart_exe(),
                Confinement::Unrestricted,
                1000,
            ),
            "fixture".into(),
        );
        let mut app = App::new(settings, preferences, shared, Size::new(80, 24), false);
        let mut runtime = Runtime::new(app.sessions());
        let attached = app.update(Msg::Attached(tasks::opened((session, "s1".into())).await));
        run_runtime_effects(&mut app, &mut runtime, attached);

        (app, runtime)
    }

    #[tokio::test]
    async fn malformed_auto_name_restores_late_input_until_resubmit() {
        tokio::time::timeout(
            Duration::from_secs(5),
            rename_case(None, NameReply::Malformed),
        )
        .await
        .unwrap();
    }

    #[derive(PartialEq)]
    enum NameReply {
        Valid,
        Refusal,
        Malformed,
    }

    async fn rename_case(interrupt: Option<bool>, name: NameReply) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = (entered.clone(), release.clone());
        let dir = crate::tool::test_support::TempDir::new("tui-rename-queue");
        let (provider, requests) = crate::provider::mock::serve_with(
            dir.path(),
            vec![
                Reply::text("reply-1"),
                if name == NameReply::Refusal {
                    let mut reply = Reply::text("");
                    reply.chunks[0]["choices"][0]["delta"] =
                        serde_json::json!({"refusal":"Cannot name this session"});
                    reply
                } else if name == NameReply::Malformed {
                    Reply::text("not name JSON")
                } else {
                    Reply::text(r#"{"name":"auto-named"}"#)
                },
                Reply::text("reply-2"),
            ],
            move |i| {
                let (entered, release) = gate.clone();
                async move {
                    if i == Some(1) {
                        entered.notify_one();
                        release.notified().await;
                    }
                }
            },
        )
        .await;
        let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;

        app.insert("first");
        app.prepare_or_commit(Delivery::Send);
        let image = InlineImage::new(PNG.to_vec(), None, "[image #1]".into()).unwrap();
        app.insert("distinct-second [image #1]");
        app.drafts.images.push(image.clone());
        app.prepare_or_commit(Delivery::Queue);
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);

        let mut started = 0;
        loop {
            let batch = runtime.events.recv().await.unwrap();
            started += batch
                .iter()
                .filter(|e| matches!(e, UiEvent::Started(_)))
                .count();
            let effects = app.update(Msg::Events(batch));
            run_runtime_effects(&mut app, &mut runtime, effects);
            if started == 2 {
                break;
            }
        }
        entered.notified().await;
        app.insert("distinct-third");
        app.prepare_or_commit(Delivery::Send);
        if let Some(resend) = interrupt {
            app.interrupt(resend);
        }
        let effects = std::mem::take(&mut app.effects);
        run_runtime_effects(&mut app, &mut runtime, effects);
        release.notify_one();
        let reclaim = interrupt == Some(false) || name != NameReply::Valid;
        let late = InlineImage::new(PNG.to_vec(), None, "[image #2]".into()).unwrap();
        loop {
            let batch = runtime.events.recv().await.unwrap();
            let done = batch.iter().any(|e| match e {
                UiEvent::Interrupted { .. } => interrupt == Some(false),
                UiEvent::Ended {
                    error: Some(message),
                    ..
                } if name == NameReply::Refusal => message.contains("refused"),
                UiEvent::Ended { .. } => name == NameReply::Malformed,
                UiEvent::Text(text) => text.contains("reply-2"),
                _ => false,
            });
            if done && reclaim {
                // The outcome exists, but the composer still belongs to the busy UI.
                app.insert("late steer [image #2]");
                app.drafts.images.push(late.clone());
                app.prepare_or_commit(Delivery::Send);
                let effects = std::mem::take(&mut app.effects);
                run_runtime_effects(&mut app, &mut runtime, effects);
            }
            let effects = app.update(Msg::Events(batch));
            run_runtime_effects(&mut app, &mut runtime, effects);
            if done {
                break;
            }
        }
        if reclaim {
            assert_eq!(requests.lock().unwrap().len(), 2);
            assert_eq!(
                app.drafts.composer.text(),
                "distinct-second [image #1]\ndistinct-third\nlate steer [image #2]"
            );
            assert_eq!(app.drafts.images, [image, late]);
            assert!(app.session_view().unwrap().inbox().is_empty());
            app.prepare_or_commit(Delivery::Send);
            let effects = std::mem::take(&mut app.effects);
            run_runtime_effects(&mut app, &mut runtime, effects);
            loop {
                let batch = runtime.events.recv().await.unwrap();
                let answered = batch
                    .iter()
                    .any(|event| matches!(event, UiEvent::Text(text) if text.contains("reply-2")));
                let effects = app.update(Msg::Events(batch));
                run_runtime_effects(&mut app, &mut runtime, effects);
                if answered {
                    break;
                }
            }
        }
        if let Some(mut active) = runtime.running.take() {
            active.driver.stop().await.unwrap();
        }
        assert_eq!(
            runtime.tasks.finish(app.sessions()).await,
            Vec::<String>::new()
        );

        let sent = requests.lock().unwrap();
        {
            assert_eq!(sent.len(), 3);
            let body = sent[2].to_string();
            assert_eq!(body.matches("late steer").count(), usize::from(reclaim));
            assert_eq!(body.matches("distinct-second").count(), 1);
            assert_eq!(body.matches("distinct-third").count(), 1);
            assert!(body.find("distinct-second") < body.find("distinct-third"));
            assert!(body.contains(PNG_BASE64));
            assert!(app.drafts.composer.is_empty());
        }
    }

    #[tokio::test]
    async fn provider_switch_before_submit_routes_prompt_to_new_provider() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = crate::tool::test_support::TempDir::new("tui-switch-order");
            let dir_a = dir.path().join("a");
            let dir_b = dir.path().join("b");
            std::fs::create_dir_all(&dir_a).unwrap();
            std::fs::create_dir_all(&dir_b).unwrap();
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let gate = (entered.clone(), release.clone());
            let mut prewarm = true;
            let (provider_a, requests_a) = crate::provider::mock::serve_with(
                &dir_a,
                vec![Reply::text("wrong-provider")],
                move |index| {
                    let wait = index.is_none() && std::mem::take(&mut prewarm);
                    let (entered, release) = gate.clone();
                    async move {
                        if wait {
                            entered.notify_one();
                            release.notified().await;
                        }
                    }
                },
            )
            .await;
            let (provider_b, requests_b) = crate::provider::mock::serve(
                &dir_b,
                vec![Reply::text("from-b"), Reply::text(r#"{"name":"switched"}"#)],
            )
            .await;
            let (mut app, mut runtime) = attached_fixture(dir.path(), provider_a).await;

            // Hold the driver in prewarm so switch and submit are pending together
            // when idle() first polls, and also verify a recalled prompt before the
            // switch cannot let a stale start token overtake the switch.
            entered.notified().await;
            app.insert("recalled-before-switch");
            app.prepare_or_commit(Delivery::Send);
            let effects = std::mem::take(&mut app.effects);
            run_runtime_effects(&mut app, &mut runtime, effects);
            let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))));
            run_runtime_effects(&mut app, &mut runtime, effects);
            assert_eq!(app.drafts.composer.text(), "recalled-before-switch");
            app.drafts.composer.clear();

            app.switch(Settings {
                provider: provider_b,
                model: "fixture".into(),
                reasoning_effort: None,
            });
            app.insert("for-provider-b");
            app.prepare_or_commit(Delivery::Send);
            let effects = std::mem::take(&mut app.effects);
            run_runtime_effects(&mut app, &mut runtime, effects);
            release.notify_one();

            loop {
                let batch = runtime.events.recv().await.unwrap();
                let done = batch
                    .iter()
                    .any(|e| matches!(e, UiEvent::Text(text) if text.contains("from-b")));
                let effects = app.update(Msg::Events(batch));
                run_runtime_effects(&mut app, &mut runtime, effects);
                if done {
                    break;
                }
            }
            runtime.running.take().unwrap().driver.stop().await.unwrap();
            assert!(runtime.tasks.finish(app.sessions()).await.is_empty());

            assert!(
                requests_a.lock().unwrap().is_empty(),
                "provider A must not receive a prompt submitted after switching to B"
            );
            let sent_b = requests_b.lock().unwrap();
            assert!(!sent_b.is_empty(), "provider B must receive the prompt");
            assert_eq!(
                sent_b
                    .iter()
                    .filter(|req| req.to_string().contains("for-provider-b"))
                    .count(),
                1
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rewind_before_submit_sends_rewound_history() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = crate::tool::test_support::TempDir::new("tui-rewind-order");
            let (provider, requests) = crate::provider::mock::serve(
                dir.path(),
                vec![
                    Reply::text("reply-1"),
                    Reply::text(r#"{"name":"named"}"#),
                    Reply::text("reply-2"),
                    Reply::text("reply-after-rewind"),
                ],
            )
            .await;
            let (mut app, mut runtime) = attached_fixture(dir.path(), provider).await;

            // Complete two turns (and auto-rename after turn 1) so we have a message to rewind.
            for (prompt, expected) in [("first-kept", "reply-1"), ("second-removed", "reply-2")] {
                app.insert(prompt);
                app.prepare_or_commit(Delivery::Send);
                let effects = std::mem::take(&mut app.effects);
                run_runtime_effects(&mut app, &mut runtime, effects);
                let mut saw_reply = false;
                loop {
                    let batch = runtime.events.recv().await.unwrap();
                    if batch
                        .iter()
                        .any(|e| matches!(e, UiEvent::Text(text) if text.contains(expected)))
                    {
                        saw_reply = true;
                    }
                    let ended = batch.iter().any(|e| matches!(e, UiEvent::Ended { .. }));
                    let effects = app.update(Msg::Events(batch));
                    run_runtime_effects(&mut app, &mut runtime, effects);
                    if saw_reply && ended {
                        break;
                    }
                }
                if prompt == "first-kept" {
                    // Wait for automatic rename to finish before turn 2.
                    loop {
                        let batch = runtime.events.recv().await.unwrap();
                        let renamed = batch.iter().any(|e| matches!(e, UiEvent::Ended { .. }));
                        let effects = app.update(Msg::Events(batch));
                        run_runtime_effects(&mut app, &mut runtime, effects);
                        if renamed {
                            break;
                        }
                    }
                }
            }

            // Queue Command::Rewind(2) (removes "second-removed" and "reply-2") and
            // immediately submit a new prompt so both are pending together in idle().
            run_runtime_effects(
                &mut app,
                &mut runtime,
                vec![Effect::Send(Command::Rewind(2))],
            );
            app.insert("after-rewind");
            app.prepare_or_commit(Delivery::Send);
            let effects = std::mem::take(&mut app.effects);
            run_runtime_effects(&mut app, &mut runtime, effects);

            loop {
                let batch = runtime.events.recv().await.unwrap();
                let done = batch.iter().any(
                    |e| matches!(e, UiEvent::Text(text) if text.contains("reply-after-rewind")),
                );
                let effects = app.update(Msg::Events(batch));
                run_runtime_effects(&mut app, &mut runtime, effects);
                if done {
                    break;
                }
            }
            runtime.running.take().unwrap().driver.stop().await.unwrap();
            assert!(runtime.tasks.finish(app.sessions()).await.is_empty());

            let sent = requests.lock().unwrap();
            assert_eq!(sent.len(), 4);
            let final_req = sent[3].to_string();
            assert!(final_req.contains("first-kept"), "{final_req}");
            assert!(final_req.contains("after-rewind"), "{final_req}");
            assert!(
                !final_req.contains("second-removed"),
                "submit overtook rewind and sent removed history: {final_req}"
            );
        })
        .await
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn settings_picker_recovers_mcp_after_unconfirmed_shutdown_without_flipping_permissions()
    {
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};

        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = crate::tool::test_support::TempDir::new("tui-mcp-retry");
            let config = dir.path().join("config");
            let project = dir.path().join("project");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::create_dir_all(&project).unwrap();

            // Hold the old HTTP MCP worker open so close() times out.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let held_url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let old_config = crate::mcp::Config::parse(
                &serde_json::json!({"mcpServers": {"fx": {"url": held_url}}}).to_string(),
            )
            .unwrap();
            let store = crate::auth::Store::new(config.join("auth.json"));
            let client = reqwest::Client::new();
            let mut preferences = Current::default();
            preferences.apply(&Change::Sandbox(true)).unwrap();
            let initial = preferences.effective.permissions.confinement().unwrap();
            let old_registry = crate::mcp::Registry::start(
                &old_config,
                crate::confinement::Place {
                    cwd: project.clone(),
                    executable: crate::bart_exe(),
                    spill_dir: dir.path().join("output"),
                    confinement: initial,
                },
                &client,
                &store,
            );

            let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = BufReader::new(stream);
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();

            // Configure mcp.json with a healthy stdio replacement that records startup.
            let marker = dir.path().join("replacement-started");
            let fixture = include_str!("../../../../bart-core/src/mcp/fixtures/server.py");
            let script = format!(
                "import pathlib; pathlib.Path({:?}).write_text('started')\n{fixture}",
                marker.to_str().unwrap()
            );
            let next_config = serde_json::json!({
                "mcpServers": {"fx": {"command": "python3", "args": ["-c", script]}}
            })
            .to_string();
            std::fs::write(crate::mcp::path(&config), next_config).unwrap();

            let context = crate::tool::Context::new(
                project.clone(),
                crate::bart_exe(),
                dir.path().join("output"),
                10_000,
                initial,
                Arc::clone(&old_registry),
            )
            .unwrap();
            let (provider, requests) = crate::provider::mock::serve(
                dir.path(),
                vec![
                    Reply::python(
                        "call_work",
                        &serde_json::json!({
                            "code": "intervening_value = 42\nkept_job = bg_run(['python3', '-c', 'import time; time.sleep(30)'])\nprint('started', intervening_value)"
                        }),
                    ),
                    Reply::text("work-done"),
                    Reply::text("mcp-retry-session"),
                    Reply::python(
                        "call_verify",
                        &serde_json::json!({
                            "code": "assert intervening_value == 42\nassert kept_job.outcome is None\nprint('SURVIVED:' + str(intervening_value) + ':running')\nprint(mcp.tools('fx'))"
                        }),
                    ),
                    Reply::text("verified-after-retry"),
                ],
            )
            .await;
            preferences
                .apply(&Change::ContextWindow {
                    provider: provider.name().to_owned(),
                    model: "fixture".into(),
                    tokens: Some(crate::preferences::ContextWindow::try_from(100_000).unwrap()),
                })
                .unwrap();
            let settings = Settings {
                provider,
                model: "fixture".into(),
                reasoning_effort: None,
            };
            let sessions = crate::record::Sessions::new(&dir.path().join("sessions"), &project);
            let session = Session::new_ephemeral(
                client.clone(),
                store.clone(),
                settings.clone(),
                preferences.clone(),
                context,
                "tui-mcp-retry".into(),
            );
            let shared = Shared {
                opener: crate::agent::Opener {
                    executable: crate::bart_exe(),
                    sessions: sessions.clone(),
                    cwd: project.clone(),
                    client: client.clone(),
                    store: store.clone(),
                },
                loader: CatalogLoader {
                    client,
                    store,
                    config: config.clone(),
                },
                sessions,
                state: dir.path().join("state.json"),
                directory: Clean::new("project"),
            };
            let mut app = App::new(settings, preferences, shared, Size::new(80, 24), false);
            let mut runtime = Runtime::new(app.sessions());
            let attached = app.update(Msg::Attached(
                tasks::opened((session, "tui-mcp-retry".into())).await,
            ));
            run_runtime_effects(&mut app, &mut runtime, attached);

            let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
            let open_settings = |app: &mut App, runtime: &mut Runtime| {
                app.insert("/settings");
                let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(key(
                    KeyCode::Enter,
                )))));
                run_runtime_effects(app, runtime, effects);
            };
            let wait_preferred = async |app: &mut App, runtime: &mut Runtime| -> bool {
                let mut replaced = false;
                loop {
                    let batch = runtime.events.recv().await.unwrap();
                    replaced |= batch
                        .iter()
                        .any(|event| matches!(event, UiEvent::RegistryReplaced));
                    let done = batch
                        .iter()
                        .any(|event| matches!(event, UiEvent::Preferred(..)));
                    let effects = app.update(Msg::Events(batch));
                    run_runtime_effects(app, runtime, effects);
                    if done {
                        return replaced;
                    }
                }
            };

            // Open /settings and apply the confinement change through SettingsPicker.
            open_settings(&mut app, &mut runtime);
            for event in [Event::Paste("Sandbox".into()), Event::Key(key(KeyCode::Enter))] {
                let effects = app.update(Msg::Input(ReadEvent::Terminal(event)));
                run_runtime_effects(&mut app, &mut runtime, effects);
            }
            let Some(Surface::Picker(picker)) = &mut app.surface else {
                panic!("settings picker should be open");
            };
            let stamp = view::FrameStamp {
                seq: 1,
                size: (80, 24),
            };
            picker.confirm_mut().expect("restart confirmation").painted = Some(stamp);
            let Choice::Prefer(change) = picker.key(key(KeyCode::Enter), Some(stamp)) else {
                panic!("expected a sandbox change");
            };
            assert_eq!(change, Change::Sandbox(false));
            app.prefer(change);
            let effects = std::mem::take(&mut app.effects);
            run_runtime_effects(&mut app, &mut runtime, effects);

            // Attempt 1: shutdown times out -> warning and explicit retry row appear.
            assert!(!wait_preferred(&mut app, &mut runtime).await);
            let warning = app.status.text().expect("expected shutdown warning");
            assert!(warning.contains("shutdown unconfirmed"), "{warning}");
            assert!(warning.contains("MCP restart"), "{warning}");
            assert!(!marker.exists());
            assert!(Arc::ptr_eq(
                &app.session.as_ref().unwrap().context.mcp(),
                &old_registry
            ));
            assert!(!app.info.preferences.effective.permissions.sandbox.enabled);
            assert_eq!(
                app.info.preferences.effective.permissions.sandbox.network,
                crate::confinement::Network::On
            );
            assert_eq!(app.info.mcp_retry, Some(Change::Sandbox(false)));
            let Some(Surface::Picker(picker)) = &app.surface else {
                panic!("settings picker should remain open");
            };
            assert_eq!(picker.menu().selected_key(), Some("MCP restart"));

            // Dismiss /settings, do intervening Python + background-job work, then reopen /settings.
            let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(key(
                KeyCode::Esc,
            )))));
            run_runtime_effects(&mut app, &mut runtime, effects);
            let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(key(
                KeyCode::Esc,
            )))));
            run_runtime_effects(&mut app, &mut runtime, effects);
            assert!(app.surface.is_none());

            let run_turn = async |app: &mut App, runtime: &mut Runtime, prompt: &str, expected: &str| {
                app.insert(prompt);
                app.prepare_or_commit(Delivery::Send);
                let effects = std::mem::take(&mut app.effects);
                run_runtime_effects(app, runtime, effects);
                let mut saw_reply = false;
                loop {
                    let batch = runtime.events.recv().await.unwrap();
                    let mut ended = false;
                    for event in &batch {
                        match event {
                            UiEvent::Text(text) => saw_reply |= text.contains(expected),
                            UiEvent::Approval(_) => panic!("unexpected approval for {prompt}"),
                            UiEvent::Ended { error, .. } => {
                                assert!(error.is_none(), "{error:?}");
                                ended = true;
                            }
                            _ => {}
                        }
                    }
                    let effects = app.update(Msg::Events(batch));
                    run_runtime_effects(app, runtime, effects);
                    if ended {
                        assert!(saw_reply, "turn ended without {expected}");
                        break;
                    }
                }
            };
            run_turn(&mut app, &mut runtime, "do intervening work", "work-done").await;
            // Wait for automatic rename to finish before opening /settings.
            loop {
                let batch = runtime.events.recv().await.unwrap();
                let renamed = batch
                    .iter()
                    .any(|event| matches!(event, UiEvent::Ended { .. }));
                let effects = app.update(Msg::Events(batch));
                run_runtime_effects(&mut app, &mut runtime, effects);
                if renamed {
                    break;
                }
            }
            assert_eq!(app.session.as_ref().unwrap().jobs.running_count(), 1);
            assert!(
                app.session
                    .as_ref()
                    .unwrap()
                    .context
                    .python_inventory()
                    .contains("intervening_value")
            );

            open_settings(&mut app, &mut runtime);
            let Some(Surface::Picker(picker)) = &app.surface else {
                panic!("settings picker should reopen");
            };
            assert_eq!(picker.menu().selected_key(), Some("MCP restart"));

            // Release the held worker and invoke retry directly with Enter.
            drop(socket);
            let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(key(
                KeyCode::Enter,
            )))));
            run_runtime_effects(&mut app, &mut runtime, effects);
            assert!(wait_preferred(&mut app, &mut runtime).await);

            assert!(app.status.text().is_none(), "{:?}", app.status.text());
            assert!(app.info.mcp_retry.is_none());
            assert!(!app.info.preferences.effective.permissions.sandbox.enabled);
            assert_eq!(
                app.info.preferences.effective.permissions.sandbox.network,
                crate::confinement::Network::On
            );
            let Some(Surface::Picker(picker)) = &app.surface else {
                panic!("settings picker should stay open after recovery");
            };
            assert_ne!(picker.menu().selected_key(), Some("MCP restart"));

            // Intervening Python state and running background job must survive MCP-only retry.
            assert_eq!(
                app.session.as_ref().unwrap().jobs.running_count(),
                1,
                "MCP-only retry must not stop intervening background jobs"
            );
            assert!(
                app.session
                    .as_ref()
                    .unwrap()
                    .context
                    .python_inventory()
                    .contains("intervening_value"),
                "MCP-only retry must not discard intervening Python state"
            );

            let recovered = app.session.as_ref().unwrap().context.mcp();
            assert!(!Arc::ptr_eq(&recovered, &old_registry));
            let tools =
                tokio::time::timeout(Duration::from_secs(10), recovered.tools(Some("fx"), None))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(tools.contains("echo"), "{tools}");
            assert!(marker.exists());

            // Close /settings and confirm the live interpreter still holds the binding,
            // sees the running job, and reaches the replacement MCP server.
            let effects = app.update(Msg::Input(ReadEvent::Terminal(Event::Key(key(
                KeyCode::Esc,
            )))));
            run_runtime_effects(&mut app, &mut runtime, effects);
            assert!(app.surface.is_none());
            run_turn(
                &mut app,
                &mut runtime,
                "verify surviving work and recovered mcp",
                "verified-after-retry",
            )
            .await;
            let final_request = requests.lock().unwrap().last().unwrap().to_string();
            assert!(
                final_request.contains("SURVIVED:42:running"),
                "{final_request}"
            );
            assert!(final_request.contains("echo"), "{final_request}");

            runtime.running.take().unwrap().driver.stop().await.unwrap();
            assert!(runtime.tasks.finish(app.sessions()).await.is_empty());
        })
        .await
        .unwrap();
    }
}
