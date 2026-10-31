use std::{io, sync::OnceLock, task::Poll, time::Duration};

use tokio::{
    signal::unix::{Signal, SignalKind, signal},
    sync::watch,
};

const GRACE: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Termination {
    Interrupt,
    Hangup,
    Terminate,
}

impl Termination {
    pub(crate) fn exit_status(self) -> u8 {
        128 + u8::try_from(self.signal().as_raw()).expect("termination signals are small numbers")
    }

    fn signal(self) -> rustix::process::Signal {
        match self {
            Self::Interrupt => rustix::process::Signal::INT,
            Self::Hangup => rustix::process::Signal::HUP,
            Self::Terminate => rustix::process::Signal::TERM,
        }
    }
}

static REQUESTED: OnceLock<Termination> = OnceLock::new();

pub(crate) fn requested() -> Option<Termination> {
    REQUESTED.get().copied()
}

#[derive(Clone)]
pub(crate) struct Terminations(watch::Receiver<Option<Termination>>);

impl Terminations {
    pub(crate) fn listen() -> io::Result<Self> {
        let ignored = ignored_at_start();
        let mut streams = Vec::new();
        for termination in [
            Termination::Interrupt,
            Termination::Hangup,
            Termination::Terminate,
        ] {
            let number = termination.signal().as_raw();
            // On Linux an ignored signal stays ignored, as `nohup` and background jobs expect.
            if ignored & (1 << (number - 1)) == 0 {
                streams.push((termination, signal(SignalKind::from_raw(number))?));
            }
        }
        let (sender, receiver) = watch::channel(None);
        tokio::spawn(async move {
            let first = next(&mut streams).await;
            let _ = REQUESTED.set(first);
            sender.send_replace(Some(first));
            let forced = tokio::select! {
                second = next(&mut streams) => second,
                () = tokio::time::sleep(GRACE) => first,
            };
            crate::tui::restore_after_panic();
            crate::terminal_text::stderr_line(format_args!(
                "bart: exiting before cleanup finished"
            ));
            std::process::exit(forced.exit_status().into());
        });
        Ok(Self(receiver))
    }

    pub(crate) async fn requested(&mut self) -> Termination {
        match self.0.wait_for(Option::is_some).await {
            Ok(requested) => requested.expect("waited for a request"),
            Err(_) => std::future::pending().await,
        }
    }
}

pub(crate) struct JobControl {
    pub(crate) stop: Signal,
    pub(crate) continued: Signal,
}

impl JobControl {
    pub(crate) fn listen() -> io::Result<Self> {
        Ok(Self {
            stop: signal(SignalKind::from_raw(rustix::process::Signal::TSTP.as_raw()))?,
            continued: signal(SignalKind::from_raw(rustix::process::Signal::CONT.as_raw()))?,
        })
    }
}

/// Stopping an orphaned process group would leave nothing to resume it.
pub(crate) fn job_control_available() -> bool {
    use rustix::process::{getpgid, getpgrp, getppid, getsid};
    getppid().is_some_and(|parent| {
        getpgid(Some(parent)).is_ok_and(|group| group != getpgrp())
            && getsid(Some(parent)).ok() == getsid(None).ok()
    })
}

pub(crate) fn stop_job() -> io::Result<()> {
    use rustix::process::{Signal, getpgrp, kill_process_group};
    kill_process_group(getpgrp(), Signal::STOP).map_err(io::Error::from)
}

fn ignored_at_start() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let mask = status
                .lines()
                .find_map(|line| line.strip_prefix("SigIgn:"))?;
            u64::from_str_radix(mask.trim(), 16).ok()
        })
        .unwrap_or(0)
}

async fn next(streams: &mut [(Termination, Signal)]) -> Termination {
    std::future::poll_fn(|context| {
        for (termination, stream) in streams.iter_mut() {
            if stream.poll_recv(context).is_ready() {
                return Poll::Ready(*termination);
            }
        }
        Poll::Pending
    })
    .await
}
