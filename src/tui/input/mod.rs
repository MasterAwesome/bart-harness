use super::ScreenBackend;
use super::{MAX_DRAFT_BYTES, Screen, THEME, palette, paste, truecolor};
use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io::{self, Stdout, Write as _, stdout},
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};
use tokio::sync::mpsc;

use crate::signals::{JobControl, Terminations};

const RAW: u8 = 1;
const TITLE: u8 = 2;
const KEYS: u8 = 4;
const SCREEN: u8 = 8;
const MOUSE: u8 = 16;
const FOCUS: u8 = 32;

/// What Bart changed in the terminal, so a normal exit and the panic hook
/// restore it once, and only what was set.
static OWNED: AtomicU8 = AtomicU8::new(0);

pub(super) fn start() -> io::Result<(Screen, InputMode, Vec<Event>)> {
    let (_, rows) = terminal::size()?;
    if rows < 3 {
        return Err(io::Error::other(
            "the interactive terminal needs at least three rows",
        ));
    }
    crate::terminal_text::install_sink();
    let input_mode = InputMode(stdout());
    enter(RAW)?;
    let keys = terminal::supports_keyboard_enhancement().unwrap_or(false);
    let (theme, pending) = palette::theme(truecolor());
    let _ = THEME.set(theme);
    let modes = TITLE | SCREEN | MOUSE | FOCUS | if keys { KEYS } else { 0 };
    enter(modes)?;
    let (width, height) = terminal::size()?;
    let screen = ratatui::Terminal::with_options(
        ScreenBackend::new(stdout()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, width, height)),
        },
    )?;
    Ok((screen, input_mode, pending))
}

fn enter(modes: u8) -> io::Result<()> {
    let mut output = stdout();
    if modes & RAW != 0 {
        terminal::enable_raw_mode()?;
        OWNED.fetch_or(RAW, Ordering::SeqCst);
        execute!(output, EnableBracketedPaste)?;
    }
    if modes & TITLE != 0 {
        write!(output, "\x1b[22;2t")?;
        OWNED.fetch_or(TITLE, Ordering::SeqCst);
    }
    if modes & KEYS != 0 {
        execute!(
            output,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        OWNED.fetch_or(KEYS, Ordering::SeqCst);
    }
    if modes & SCREEN != 0 {
        execute!(output, EnterAlternateScreen)?;
        OWNED.fetch_or(SCREEN, Ordering::SeqCst);
    }
    if modes & MOUSE != 0 {
        execute!(output, EnableMouseCapture)?;
        OWNED.fetch_or(MOUSE, Ordering::SeqCst);
    }
    if modes & FOCUS != 0 {
        execute!(output, EnableFocusChange)?;
        OWNED.fetch_or(FOCUS, Ordering::SeqCst);
    }
    Ok(())
}

pub(super) fn suspend(screen: &mut Screen) -> io::Result<()> {
    let owned = OWNED.load(Ordering::SeqCst);
    restore_input(&mut stdout(), &OWNED);
    crate::signals::stop_job()?;
    enter(owned)?;
    screen.clear()
}

pub(super) fn reassert(screen: &mut Screen) -> io::Result<()> {
    if OWNED.load(Ordering::SeqCst) & RAW != 0 {
        terminal::enable_raw_mode()?;
    }
    screen.clear()
}

pub(super) struct InputMode(Stdout);

impl Drop for InputMode {
    fn drop(&mut self) {
        restore_input(&mut self.0, &OWNED);
        crate::terminal_text::remove_sink();
    }
}

pub(crate) fn restore_after_panic() {
    let mut output = stdout();
    if OWNED.load(Ordering::SeqCst) & SCREEN != 0 {
        let _ = execute!(output, terminal::EndSynchronizedUpdate);
    }
    restore_input(&mut output, &OWNED);
    crate::terminal_text::remove_sink();
}

fn restore_input(output: &mut impl io::Write, owned: &AtomicU8) {
    let owned = owned.swap(0, Ordering::SeqCst);
    if owned == 0 {
        return;
    }
    if owned & FOCUS != 0 {
        let _ = execute!(output, DisableFocusChange);
    }
    if owned & MOUSE != 0 {
        let _ = execute!(output, DisableMouseCapture);
    }
    if owned & SCREEN != 0 {
        let _ = execute!(output, LeaveAlternateScreen);
    }
    if owned & KEYS != 0 {
        let _ = execute!(output, PopKeyboardEnhancementFlags);
    }
    if owned & TITLE != 0 {
        let _ = write!(output, "\x1b[23;2t");
    }
    let _ = execute!(output, DisableBracketedPaste, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

#[derive(Debug)]
pub(super) enum ReadEvent {
    Terminal(Event),
    Refused(&'static str),
    Suspend,
    Continued,
    Terminate,
    Resized,
}

pub(super) struct InputReader {
    events: mpsc::Receiver<io::Result<ReadEvent>>,
    pending: std::collections::VecDeque<io::Result<ReadEvent>>,
    wake: std::os::unix::net::UnixStream,
    paused: mpsc::UnboundedReceiver<()>,
    resize: tokio::signal::unix::Signal,
    thread: Option<std::thread::JoinHandle<()>>,
    terminations: Option<Terminations>,
    job_control: JobControl,
}

impl InputReader {
    pub(super) async fn pause(&mut self) -> io::Result<()> {
        self.wake.write_all(b"p")?;
        loop {
            tokio::select! {
                ack = self.paused.recv() => return ack.ok_or_else(|| io::Error::other("terminal input reader closed")),
                event = self.events.recv() => {
                    let event = event.ok_or_else(|| io::Error::other("terminal input reader closed"))?;
                    self.pending.push_back(event);
                }
            }
        }
    }

    pub(super) fn resume(&mut self) -> io::Result<()> {
        self.wake.write_all(b"r")
    }

    pub(super) async fn recv(&mut self) -> io::Result<ReadEvent> {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        let termination = async {
            match &mut self.terminations {
                Some(terminations) => terminations.requested().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = termination => {
                // Delivered once; the shutdown it starts must not be restarted.
                self.terminations = None;
                Ok(ReadEvent::Terminate)
            }
            Some(()) = self.job_control.stop.recv() => Ok(ReadEvent::Suspend),
            Some(()) = self.job_control.continued.recv() => Ok(ReadEvent::Continued),
            Some(()) = self.resize.recv() => Ok(ReadEvent::Resized),
            event = self.events.recv() => {
                event.unwrap_or_else(|| Err(io::Error::other("terminal input reader closed")))
            }
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.events.close();
        let _ = self.wake.write_all(b"q");
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn read_input(
    pending: Vec<Event>,
    terminations: Terminations,
    job_control: JobControl,
) -> io::Result<InputReader> {
    use filedescriptor::{POLLERR, POLLHUP, POLLIN, poll, pollfd};
    use std::{io::Read as _, os::fd::AsRawFd as _};
    let terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    let (wake, mut commands) = std::os::unix::net::UnixStream::pair()?;
    commands.set_nonblocking(true)?;
    let resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let (sender, events) = mpsc::channel(8);
    let (ack, paused) = mpsc::unbounded_channel();
    let thread = std::thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let mut pending = pending.into_iter();
            let mut stopped = false;
            loop {
                if sender.is_closed() {
                    return Ok(());
                }
                let mut command = [0];
                match commands.read(&mut command) {
                    Ok(0) => return Ok(()),
                    Ok(_) => match command[0] {
                        b'q' => return Ok(()),
                        b'p' => {
                            stopped = true;
                            let _ = ack.send(());
                        }
                        b'r' => stopped = false,
                        _ => {}
                    },
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
                if !stopped {
                    // The use-dev-tty backend skips its parser when the timeout is zero.
                    let next = pending.next().map(Ok).or_else(|| {
                        match event::poll(Duration::from_millis(1)) {
                            Ok(true) => Some(event::read()),
                            Ok(false) => None,
                            Err(error) => Some(Err(error)),
                        }
                    });
                    if let Some(event) = next {
                        let event = match event? {
                            Event::Resize(..) => continue,
                            Event::Paste(text)
                                if paste::normalized_bytes(&text) > MAX_DRAFT_BYTES =>
                            {
                                ReadEvent::Refused(
                                    "paste exceeds the 1 MiB draft limit; paste refused",
                                )
                            }
                            Event::Paste(text) => ReadEvent::Terminal(Event::Paste(
                                paste::normalize(&text).into_owned(),
                            )),
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('z'),
                                modifiers: KeyModifiers::CONTROL,
                                kind: KeyEventKind::Press,
                                ..
                            }) => ReadEvent::Suspend,
                            event => ReadEvent::Terminal(event),
                        };
                        if sender.blocking_send(Ok(event)).is_err() {
                            return Ok(());
                        }
                        continue;
                    }
                }
                let mut fds = [
                    pollfd {
                        fd: commands.as_raw_fd(),
                        events: POLLIN,
                        revents: 0,
                    },
                    pollfd {
                        fd: terminal.as_raw_fd(),
                        events: if stopped { 0 } else { POLLIN },
                        revents: 0,
                    },
                ];
                match poll(&mut fds, None) {
                    Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error))
                        if error.kind() == io::ErrorKind::Interrupted =>
                    {
                        continue;
                    }
                    result => {
                        result.map_err(io::Error::other)?;
                    }
                }
                if fds[1].revents & (POLLHUP | POLLERR) != 0
                    || (fds[1].revents & POLLIN != 0 && rustix::io::write(&terminal, &[]).is_err())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the terminal closed",
                    ));
                }
            }
        })();
        if let Err(error) = result {
            let _ = sender.blocking_send(Err(error));
        }
    });
    Ok(InputReader {
        events,
        pending: Default::default(),
        wake,
        paused,
        resize,
        thread: Some(thread),
        terminations: Some(terminations),
        job_control,
    })
}

#[cfg(test)]
mod tests;
