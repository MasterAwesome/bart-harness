use super::session::Command;
use super::tasks::{Kind, Ticket};
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
use ratatui::text::{Line, Text};
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
}
struct Turn {
    progress: progress::Progress,
    started: Instant,
    animation: Animation,
}
impl Turn {
    fn new() -> Self {
        Self {
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
struct PendingInput {
    input: Input,
    display: Clean,
}
impl PendingInput {
    fn new(input: Input) -> Self {
        let display = Clean::new(&input.text);
        Self { input, display }
    }
}
struct Drafts {
    composer: Composer,
    queued: Vec<PendingInput>,
    steers: Vec<PendingInput>,
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
impl Default for Drafts {
    fn default() -> Self {
        Self {
            composer: Composer::default(),
            queued: Vec::new(),
            steers: Vec::new(),
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
    preferences: Option<Ticket>,
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
    Steering(Result<Input, Input>),
    Reclaimed(Vec<Input>),
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
    Steer(Input),
    Reclaim,
    Spawn(Ticket, Task),
    Cancel(Kind),
    Copy(String, String),
    Clear,
    Suspend,
    Reassert,
    RebindRegistry,
    Quit,
    Exec(crate::relaunch::Plan),
    Open(Box<(Session, String)>, Ticket),
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
            Msg::Steering(result) => match result {
                Ok(input) => {
                    self.drafts.steers.push(PendingInput::new(input));
                }
                Err(input) => {
                    if self.restore_draft(&input.text) {
                        self.drafts.images = input.images;
                    } else {
                        self.drafts.queued.insert(0, PendingInput::new(input));
                    }
                    self.set_status(StatusKind::Info, INPUT_FULL);
                }
            },
            Msg::Reclaimed(unsent) => self.restore_steers(unsent),
            Msg::Clipboard(result) => match result {
                Ok(text) => self.set_status(StatusKind::Info, text),
                Err(error) => self.set_status(StatusKind::Error, error.to_string()),
            },
            Msg::Size(size) => self.resized(size),
            Msg::CommandRefused(command) => {
                match command {
                    Command::Submit(input) => {
                        self.drafts.queued.insert(0, PendingInput::new(input));
                        self.turn = None;
                    }
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
