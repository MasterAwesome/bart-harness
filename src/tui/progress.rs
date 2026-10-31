use std::time::{Duration, Instant, SystemTime};

use super::session::UiEvent;

#[derive(PartialEq, Eq)]
enum Phase {
    Waiting,
    Compacting,
    Reasoning,
    Generating,
    ToolCall,
    Python,
    Retry(Instant),
    Reset(SystemTime),
}

pub(super) struct Progress {
    phase: Phase,
    since: Instant,
    received: bool,
    rate: Rate,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Waiting,
            since: Instant::now(),
            received: false,
            rate: Rate::default(),
        }
    }
}

impl Progress {
    pub(super) fn waiting_for_reset(&self) -> bool {
        matches!(self.phase, Phase::Reset(..))
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    fn enter(&mut self, phase: Phase) {
        if self.phase != phase {
            *self = Self {
                phase,
                ..Self::default()
            };
        }
    }

    pub(super) fn activity(&mut self, activity: &UiEvent) {
        let now = Instant::now();
        match activity {
            UiEvent::Text(text) => self.received(Phase::Generating, text.len()),
            UiEvent::Reasoning(text) => self.received(Phase::Reasoning, text.len()),
            UiEvent::ToolBytes(bytes) => self.received(Phase::ToolCall, *bytes),
            UiEvent::ToolStarted { .. } => self.enter(Phase::Python),
            UiEvent::Compacting => self.enter(Phase::Compacting),
            UiEvent::CompactingBytes(bytes) => self.received(Phase::Compacting, *bytes),
            UiEvent::CompactionNoop => self.reset(),
            UiEvent::Responded if self.phase == Phase::Compacting => {}
            UiEvent::ToolFinished { .. } | UiEvent::Compacted(..) | UiEvent::Responded => {
                self.reset()
            }
            UiEvent::WaitingForReset { delay, .. } => {
                self.enter(Phase::Reset(SystemTime::now() + *delay))
            }
            UiEvent::Retrying { delay, .. } => self.enter(Phase::Retry(now + *delay)),
            _ => {}
        }
    }

    fn received(&mut self, phase: Phase, bytes: usize) {
        if bytes != 0 {
            self.enter(phase);
            self.received = true;
            self.rate.add(bytes);
        }
    }

    pub(super) fn compact(&self) -> &'static str {
        match self.phase {
            Phase::Waiting => "wait",
            Phase::Compacting => "compact",
            Phase::Reasoning => "think",
            Phase::Generating => "gen",
            Phase::ToolCall => "tool",
            Phase::Python => "Python",
            Phase::Retry(_) => "retry",
            Phase::Reset(_) => "reset",
        }
    }

    pub(super) fn tick(&mut self) {
        self.rate.advance(self.rate.started.elapsed());
        let now = Instant::now();
        let resumed = match self.phase {
            Phase::Retry(until) => until <= now,
            Phase::Reset(until) => until <= SystemTime::now(),
            _ => false,
        };
        if resumed {
            self.enter(Phase::Waiting);
        }
    }
    pub(super) fn status(&self, total: Duration) -> String {
        let now = Instant::now();
        let in_phase = now.saturating_duration_since(self.since).as_secs();
        let state = match self.phase {
            Phase::Waiting => format!("waiting · {in_phase}s"),
            Phase::Compacting if !self.received => format!("compacting · {in_phase}s"),
            Phase::Compacting | Phase::Generating | Phase::ToolCall => {
                let label = match self.phase {
                    Phase::Compacting => "compacting",
                    Phase::Generating => "generating",
                    Phase::ToolCall => "writing tool call",
                    _ => unreachable!(),
                };
                format!("{label} · ~{:.0} t/s · {in_phase}s", self.rate.speed())
            }
            Phase::Reasoning => format!("reasoning · {in_phase}s"),
            Phase::Python => format!("running Python · {in_phase}s"),
            Phase::Retry(until) => format!(
                "retrying in {}s",
                until.saturating_duration_since(now).as_secs()
            ),
            Phase::Reset(until) => {
                let seconds = until
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_add(1);
                format!(
                    "quota reset in {}h {:02}m {:02}s",
                    seconds / 3600,
                    seconds % 3600 / 60,
                    seconds % 60
                )
            }
        };
        format!("{state} · total {}s", total.as_secs())
    }
}

const BUCKETS: usize = 20;
const INTERVAL_MS: u128 = 100;
const WINDOW: Duration = Duration::from_secs(2);

struct Rate {
    started: Instant,
    tick: usize,
    bytes: [u32; BUCKETS],
}

impl Default for Rate {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            tick: 0,
            bytes: [0; BUCKETS],
        }
    }
}

impl Rate {
    fn add(&mut self, bytes: usize) {
        self.advance(self.started.elapsed());
        let at = self.tick % BUCKETS;
        self.bytes[at] = self.bytes[at].saturating_add(u32::try_from(bytes).unwrap_or(u32::MAX));
    }

    fn advance(&mut self, elapsed: Duration) {
        let tick = usize::try_from(elapsed.as_millis() / INTERVAL_MS).unwrap_or(usize::MAX);
        let gap = tick.saturating_sub(self.tick);
        if gap >= BUCKETS {
            self.bytes.fill(0);
        } else {
            for offset in 1..=gap {
                self.bytes[(self.tick + offset) % BUCKETS] = 0;
            }
        }
        self.tick = tick;
    }

    fn speed(&self) -> f64 {
        let elapsed = self.started.elapsed();
        let bytes = self.bytes.iter().copied().fold(0u32, u32::saturating_add);
        f64::from(bytes)
            / 4.0
            / elapsed
                .min(WINDOW)
                .max(Duration::from_millis(100))
                .as_secs_f64()
    }
}
