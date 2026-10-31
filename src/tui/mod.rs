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

use app::{App, Effect, Msg};
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

struct Runtime {
    tasks: Tasks,
    running: Option<Running>,
    events: session::Receiver,
    registry_changes: Option<tokio::sync::watch::Receiver<()>>,
    sessions: Sessions,
}

async fn interact(
    screen: &mut Screen,
    mut app: App,
    input: &mut input::InputReader,
    effects: Vec<Effect>,
) -> io::Result<Option<Next>> {
    let (_, events) = session::channel();
    let mut runtime = Runtime {
        tasks: Tasks::default(),
        running: None,
        events,
        registry_changes: None,
        sessions: app.sessions(),
    };
    let mut dirty = true;
    let mut next_frame = std::time::Instant::now();
    let mut next_spin = next_frame;
    let mut title = String::new();
    let mut effects: std::collections::VecDeque<_> = effects.into();
    let result = async {
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
    if let Some(mut active) = runtime.running {
        active.driver.stop().await;
    }
    runtime.tasks.finish().await;
    if let Ok(Some(Next::Quit(id))) = &result {
        let id = id.clone();
        if tokio::task::spawn_blocking(move || runtime.sessions.delete_if_empty(&id))
            .await
            .is_ok_and(|result| matches!(result, Ok(false)))
        {
            return Ok(None);
        }
    }
    result
}

async fn run_effects(
    screen: &mut Screen,
    input: &mut input::InputReader,
    app: &mut App,
    runtime: &mut Runtime,
    effects: &mut std::collections::VecDeque<Effect>,
) -> io::Result<Option<Effect>> {
    while let Some(effect) = effects.pop_front() {
        match effect {
            Effect::Spawn(ticket, task) => runtime.tasks.start(ticket, task),
            Effect::Cancel(kind) => runtime.tasks.cancel(kind),
            Effect::Send(command) => {
                let refused = match &runtime.running {
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
            Effect::Steer(input) => {
                let result = match app.session_view() {
                    Some(session) => session.inbox().push(input.clone()).map(|()| input),
                    None => Err(input),
                };
                effects.extend(app.update(Msg::Steering(result)));
            }
            Effect::Reclaim => {
                if let Some(session) = app.session_view() {
                    effects.extend(app.update(Msg::Reclaimed(session.inbox().take())));
                }
            }
            Effect::Interrupt { resend } => {
                if let Some(active) = &runtime.running {
                    active.control.interrupt(resend);
                }
            }
            Effect::Copy(text, what) => {
                effects.extend(app.update(Msg::Clipboard(clipboard::copy(&text, &what))))
            }
            Effect::Clear => screen.clear()?,
            Effect::Suspend => {
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
            Effect::Reassert => input::reassert(screen)?,
            Effect::RebindRegistry => {
                runtime.registry_changes = app.session_view().map(|s| s.registry().changes())
            }
            Effect::Quit => return Ok(Some(Effect::Quit)),
            Effect::Exec(plan) => return Ok(Some(Effect::Exec(plan))),
            Effect::Open(next, ticket) => {
                if let Some(old) = runtime.running.take() {
                    runtime.tasks.start(
                        ticket,
                        tasks::Task::Close {
                            ticket,
                            driver: old.driver,
                            sessions: runtime.sessions.clone(),
                            id: old.id,
                        },
                    );
                }
                let (commands, receiver) = mpsc::channel(32);
                let (sender, updates) = session::channel();
                runtime.events = updates;
                let control = sender.control.clone();
                let driver = session::Driver::new(next.0, receiver, sender, app.state_path());
                runtime.running = Some(Running {
                    driver,
                    commands,
                    control,
                    id: next.1,
                });
                runtime.registry_changes = app.session_view().map(|s| s.registry().changes());
                screen.clear()?;
            }
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
