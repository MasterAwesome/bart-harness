use crate::tui::text::Clean;
use crate::{
    agent::Settings,
    auth::Store,
    choice,
    preferences::Current,
    provider::Provider,
    record::{Resumed, Sessions},
};

use crossterm::{execute, terminal};
use ratatui::{Terminal, style::Color};
use reqwest::Client;
use std::{
    collections::VecDeque,
    io::{self, IsTerminal as _, Stdout},
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

mod app;
mod clipboard;
mod composer;
mod highlight;
mod input;
mod markdown;
mod mention;
mod palette;
mod paste;
mod picker;
mod progress;
mod scrollback;
mod selection;
mod session;
mod status;
mod tasks;
mod text;
mod transcript;

mod viewer;
mod widgets;

use app::{App, Effect, InputBoundary, Msg};
use tasks::{CatalogLoader, Shared, Tasks};
enum Next {
    Quit(String),
    Exec(crate::relaunch::Plan),
}
pub(crate) use input::restore_after_panic;
use input::{ReadEvent, read_input, start};

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const MAX_DRAFT_BYTES: usize = 1024 * 1024;

type ScreenBackend = ratatui_crossterm::CrosstermBackend<Stdout>;
type Screen = Terminal<ScreenBackend>;

fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|value| value == "truecolor" || value == "24bit")
}

static THEME: std::sync::OnceLock<palette::Theme> = std::sync::OnceLock::new();

fn theme() -> palette::Theme {
    THEME.get().copied().unwrap_or_default()
}

fn brand() -> Color {
    theme().brand
}

pub(crate) struct Launch {
    pub(crate) settings: Option<Settings>,
    pub(crate) preferences: Current,
    pub(crate) resumed: Option<Resumed>,
    pub(crate) opener: crate::agent::Opener,
    pub(crate) sessions: Sessions,
    pub(crate) client: Client,
    pub(crate) store: Store,
    pub(crate) config: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) temporary: bool,
}

pub(crate) async fn run(
    launch: Launch,
    terminations: crate::signals::Terminations,
) -> Result<(), String> {
    let failed = |error: io::Error| format!("terminal error: {error}");
    let job_control = crate::signals::JobControl::listen().map_err(failed)?;
    let (mut screen, input_mode, pending) = start().map_err(failed)?;
    let mut input = match read_input(pending, terminations, job_control) {
        Ok(input) => input,
        Err(error) => {
            finish(screen);
            return Err(failed(error));
        }
    };
    let cwd = launch.opener.cwd.clone();
    let temporary = launch.temporary;
    let result = launched(&mut screen, launch, &mut input).await;
    drop(input);
    finish(screen);
    drop(input_mode);
    match result? {
        Some(Next::Exec(plan)) => {
            if temporary {
                return Err("cannot switch directories in a temporary run".into());
            }
            crate::auth::refresh::drain().await;
            Err(format!("cannot switch session: {}", plan.execute()))
        }
        exit @ (Some(Next::Quit(_)) | None) => {
            if temporary {
                return Ok(());
            }
            if let Some(Next::Quit(id)) = exit {
                resume_command(format_args!("$ bart --session {id}"));
            } else {
                crate::terminal_text::stdout_line(format_args!(
                    "To resume the latest session in {}:",
                    cwd.display()
                ));
                resume_command(format_args!("$ bart --resume"));
            }
            Ok(())
        }
    }
}

fn resume_command(command: std::fmt::Arguments<'_>) {
    let styled = io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_some_and(|term| term != "dumb");
    if styled {
        let _ = execute!(
            io::stdout(),
            crossterm::style::SetAttribute(crossterm::style::Attribute::Italic)
        );
    }
    crate::terminal_text::stdout_line(command);
    if styled {
        let _ = execute!(
            io::stdout(),
            crossterm::style::SetAttribute(crossterm::style::Attribute::NoItalic)
        );
    }
}

async fn launched(
    screen: &mut Screen,
    launch: Launch,
    input: &mut input::InputReader,
) -> Result<Option<Next>, String> {
    let failed = |error: io::Error| format!("terminal error: {error}");
    let Launch {
        settings,
        preferences,
        resumed,
        opener,
        sessions,
        client,
        store,
        config,
        state,
        temporary: _,
    } = launch;
    let loader = CatalogLoader {
        client,
        store,
        config,
    };
    let first_run = settings.is_none();
    let initial = if let Some(settings) = settings.clone() {
        Some(opener.open(settings, preferences.clone(), resumed).await?)
    } else {
        None
    };
    let shared = Shared {
        loader,
        opener,
        sessions,
        state,
        directory: Clean::new(&crate::terminal_text::safe_text(
            &std::env::current_dir()
                .ok()
                .and_then(|cwd| {
                    cwd.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
            crate::terminal_text::TextMode::Diagnostic,
        )),
    };
    let placeholder = settings.unwrap_or(Settings {
        provider: Provider::named("chatgpt", Path::new(""))
            .map_err(io::Error::other)
            .map_err(failed)?,
        model: String::new(),
        reasoning_effort: None,
    });
    let mut app = App::new(
        placeholder,
        preferences,
        shared,
        screen.size().map_err(failed)?,
        first_run,
    );
    let effects = match initial {
        Some(opened) => app.update(Msg::Attached(tasks::opened(opened).await)),
        None => app.update(Msg::Begin),
    };
    interact(screen, app, input, effects).await.map_err(failed)
}

struct Running {
    driver: session::Driver,
    commands: mpsc::Sender<session::Command>,
    control: session::Control,
    id: String,
}

enum HostEffect {
    Copy(String, String),
    Clear,
    Suspend,
    Reassert,
    Quit,
    Exec(crate::relaunch::Plan),
}

struct Runtime {
    tasks: Tasks,
    boundary_acks: Vec<tokio::sync::oneshot::Sender<()>>,
    running: Option<Running>,
    events: session::Receiver,
    registry_changes: Option<tokio::sync::watch::Receiver<()>>,
    sessions: Sessions,
}

impl Runtime {
    fn new(sessions: Sessions) -> Self {
        let (_, events) = session::channel();
        Self {
            tasks: Tasks::default(),
            boundary_acks: Vec::new(),
            running: None,
            events,
            registry_changes: None,
            sessions,
        }
    }

    fn next_effect(&mut self, app: &mut App, effects: &mut VecDeque<Effect>) -> Option<HostEffect> {
        while let Some(effect) = effects.pop_front() {
            if let Some(host) = self.apply(app, effect, effects) {
                return Some(host);
            }
        }
        // Releasing the driver here also covers effects produced by the boundary,
        // such as enqueueing the next Tab submission or cancelling preparation.
        for ack in self.boundary_acks.drain(..) {
            let _ = ack.send(());
        }
        None
    }

    fn apply(
        &mut self,
        app: &mut App,
        effect: Effect,
        effects: &mut VecDeque<Effect>,
    ) -> Option<HostEffect> {
        match effect {
            Effect::Spawn(ticket, task) => self.tasks.start(ticket, task),
            Effect::Cancel(kind) => self.tasks.cancel(kind),
            Effect::Send(command) => {
                let refused = match &self.running {
                    Some(active) => active
                        .commands
                        .try_send(command)
                        .err()
                        .map(|error| error.into_inner()),
                    None => Some(command),
                };
                if let Some(command) = refused {
                    effects.extend(app.update(Msg::CommandRefused(command)));
                }
            }
            Effect::Approve { answer, allow } => answer.send(allow),
            Effect::Enqueue { pending, start } => {
                let accepted = self.running.as_ref().zip(app.session_view()).is_some_and(
                    |(active, session)| {
                        active.control.enqueue(
                            session.inbox(),
                            &active.commands,
                            pending.input.clone(),
                            start,
                        )
                    },
                );
                effects.extend(app.update(Msg::Steering {
                    pending,
                    accepted,
                    start,
                }));
            }
            Effect::RecallSteer { input, composer } => {
                let removed = self.running.as_ref().zip(app.session_view()).is_some_and(
                    |(active, session)| active.control.recall(session.inbox(), &input),
                );
                effects.extend(app.update(Msg::SteerRecalled(removed.then_some(composer))));
            }
            Effect::InputBoundary { next, ack } => {
                let session = app.session_view().expect("active session");
                let active = self.running.as_ref().expect("active driver");
                match next {
                    InputBoundary::Recover(mut rejected) => {
                        rejected.extend(active.control.recover_input(session.inbox()));
                        effects.extend(app.update(Msg::InputRecovered(rejected)));
                    }
                    InputBoundary::Advance | InputBoundary::Resend => {
                        match active
                            .control
                            .resume_input(session.inbox(), &active.commands)
                        {
                            session::Resume::Started => {}
                            session::Resume::Empty => {
                                if matches!(next, InputBoundary::Advance) {
                                    effects.extend(app.update(Msg::AdvanceQueue));
                                }
                            }
                            session::Resume::Refused => {
                                let recovered = active.control.recover_input(session.inbox());
                                effects.extend(app.update(Msg::InputRecovered(recovered)));
                                effects.extend(
                                    app.update(Msg::CommandRefused(session::Command::StartTurn(0))),
                                );
                            }
                        }
                    }
                }
                self.boundary_acks.push(ack);
            }
            Effect::Interrupt { resend } => {
                if let Some(active) = &self.running {
                    active.control.interrupt(resend);
                }
            }
            Effect::RebindRegistry => {
                self.registry_changes = app.session_view().map(|s| s.registry().changes());
            }
            Effect::Open {
                next,
                close: ticket,
                delete_previous,
            } => {
                let old = self.running.take();
                let (commands, receiver) = mpsc::channel(32);
                let (sender, updates) = session::channel();
                self.events = updates;
                let control = sender.control.clone();
                let driver = session::Driver::new(next.0, receiver, sender, app.state_path());
                self.running = Some(Running {
                    driver,
                    commands,
                    control,
                    id: next.1,
                });
                if let Some(old) = old {
                    self.tasks.start(
                        ticket,
                        tasks::Task::Close {
                            ticket,
                            driver: old.driver,
                            sessions: self.sessions.clone(),
                            delete: delete_previous.as_ref() == Some(&old.id),
                            id: old.id,
                        },
                    );
                }

                self.registry_changes = app.session_view().map(|s| s.registry().changes());
                return Some(HostEffect::Clear);
            }
            Effect::Copy(text, what) => return Some(HostEffect::Copy(text, what)),
            Effect::Clear => return Some(HostEffect::Clear),
            Effect::Suspend => return Some(HostEffect::Suspend),
            Effect::Reassert => return Some(HostEffect::Reassert),
            Effect::Quit => return Some(HostEffect::Quit),
            Effect::Exec(plan) => return Some(HostEffect::Exec(plan)),
        }
        None
    }
}

async fn interact(
    screen: &mut Screen,
    mut app: App,
    input: &mut input::InputReader,
    effects: Vec<Effect>,
) -> io::Result<Option<Next>> {
    let mut runtime = Runtime::new(app.sessions());
    let mut dirty = true;
    let mut next_frame = std::time::Instant::now();
    let mut next_spin = next_frame;
    let mut title = String::new();
    let mut effects: VecDeque<_> = effects.into();
    let mut result = async {
        loop {
            match run_effects(screen, input, &mut app, &mut runtime, &mut effects).await? {
                Some(Effect::Quit) => return Ok(app.session_view().map(|s| Next::Quit(s.id().to_owned()))),
                Some(Effect::Exec(plan)) => return Ok(Some(Next::Exec(plan))),
                _ => {},
            }
            if dirty && std::time::Instant::now() >= next_frame {
                let size = terminal::size()?.into();
                effects.extend(app.update(Msg::Size(size)));
                let jobs = app.session_view().map_or(0, |s| s.running_jobs());
                effects.extend(app.update(Msg::PrepareFrame));
                if app.drawable() {
                    let painted = app.draw(screen, jobs);
                    match painted {
                        Ok(painted) => { effects.extend(app.update(Msg::Painted(Some(painted)))); crate::terminal_text::line_shown(); },
                        Err(error) => { app.update(Msg::Painted(None)); return Err(error); },
                    }
                }
                let current_title = app.title();
                if current_title != title { use std::io::Write as _; write!(io::stdout(), "\x1b]2;bart · {current_title}\x1b\\")?; io::stdout().flush()?; title = current_title; }
                next_frame = std::time::Instant::now() + std::time::Duration::from_millis(16);
                dirty = false;
            }
            let now = std::time::Instant::now();
            if app.spinning() && next_spin <= now { next_spin = now + std::time::Duration::from_millis(100); }
            let deadline = app.deadline(dirty, next_frame, next_spin);
            let msg = tokio::select! {
                _ = crate::terminal_text::line_ready() => Msg::Wake,
                _ = async { match app.session_view() { Some(s) => s.live().changed().await, None => std::future::pending().await } }, if !dirty => Msg::Wake,
                event = input.recv() => Msg::Input(event?),
                Some(batch) = runtime.events.recv() => Msg::Events(batch),
                Some(done) = runtime.tasks.next() => Msg::Done(done),
                changed = async { match &mut runtime.registry_changes { Some(changes) => changes.changed().await, None => std::future::pending().await } } => { if changed.is_err() { runtime.registry_changes = None; } Msg::RegistryChanged },
                _ = async { match deadline { Some(at) => tokio::time::sleep_until(at.into()).await, None => std::future::pending().await } } => Msg::Timer,
                error = async { match &mut runtime.running { Some(active) => active.driver.ended().await, None => std::future::pending().await } } => return Err(error),
            };
            effects.extend(app.update(msg));
            dirty = true;
        }
    }.await;
    let mut failures = Vec::new();
    let mut closed = None;
    if let Some(mut active) = runtime.running {
        match active.driver.stop().await {
            Ok(()) => closed = Some(active.id),
            Err(error) => failures.push(format!("session {}: {error}", active.id)),
        }
    }
    failures.extend(runtime.tasks.finish(app.sessions()).await);
    if let Ok(Some(Next::Quit(id))) = &result
        && closed.as_ref() == Some(id)
    {
        match tasks::delete_if_empty(runtime.sessions, id).await {
            Ok(false) => result = Ok(None),
            Ok(true) => {}
            Err(error) => failures.push(error),
        }
    }
    if failures.is_empty() {
        result
    } else {
        if let Err(error) = result {
            failures.insert(0, error.to_string());
        }
        Err(io::Error::other(failures.join("; ")))
    }
}

async fn run_effects(
    screen: &mut Screen,
    input: &mut input::InputReader,
    app: &mut App,
    runtime: &mut Runtime,
    effects: &mut VecDeque<Effect>,
) -> io::Result<Option<Effect>> {
    while let Some(host) = runtime.next_effect(app, effects) {
        match host {
            HostEffect::Copy(text, what) => {
                effects.extend(app.update(Msg::Clipboard(clipboard::copy(&text, &what))))
            }
            HostEffect::Clear => screen.clear()?,
            HostEffect::Suspend => {
                input.pause().await?;
                if crate::signals::job_control_available() {
                    input::suspend(screen)?;
                } else {
                    effects.extend(app.update(Msg::Input(ReadEvent::Refused(
                        "not suspended: no shell job control to resume Bart",
                    ))));
                }
                input.resume()?;
            }
            HostEffect::Reassert => input::reassert(screen)?,
            HostEffect::Quit => return Ok(Some(Effect::Quit)),
            HostEffect::Exec(plan) => return Ok(Some(Effect::Exec(plan))),
        }
    }
    Ok(None)
}

async fn remember(state: &Path, settings: &Settings) -> Result<(), String> {
    let state = state.to_owned();
    let choice = settings.choice();
    tokio::task::spawn_blocking(move || choice::save(&state, &choice))
        .await
        .map_err(|error| format!("cannot remember this choice: {error}"))?
        .map_err(remember_warning)
}

fn remember_warning(error: crate::persist::ReplaceError) -> String {
    match error {
        crate::persist::ReplaceError::Unchanged(error) => {
            format!("using this choice now; could not remember it: {error}")
        }
        crate::persist::ReplaceError::Replaced(error) => {
            format!("choice remembered; crash durability is uncertain: {error}")
        }
    }
}

fn finish(mut screen: Screen) {
    if screen.show_cursor().is_err() {
        std::mem::forget(screen);
    }
}

fn synchronized<T>(
    screen: &mut Screen,
    draw: impl FnOnce(&mut Screen) -> io::Result<T>,
) -> io::Result<T> {
    execute!(screen.backend_mut(), terminal::BeginSynchronizedUpdate)?;
    let result = draw(screen);
    let end = execute!(screen.backend_mut(), terminal::EndSynchronizedUpdate);
    result.and_then(|value| end.map(|()| value))
}
