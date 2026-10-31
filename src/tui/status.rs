use crate::tui::text::Clean;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Kind {
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Default)]
pub(super) struct Status {
    text: Clean,
    kind: Kind,
    until: Option<Instant>,
}

impl Status {
    pub(super) fn set(&mut self, kind: Kind, text: Clean, duration: Option<Duration>) {
        self.text = text;
        self.kind = kind;
        self.until = duration.map(|duration| Instant::now() + duration);
    }
    pub(super) fn text(&self) -> Option<&str> {
        (!self.text.is_empty() && self.until.is_none_or(|until| Instant::now() < until))
            .then_some(&self.text)
    }
    pub(super) fn kind(&self) -> Kind {
        self.kind
    }
    pub(super) fn until(&self) -> Option<Instant> {
        self.until.filter(|_| self.text().is_some())
    }
    pub(super) fn persistent(&self) -> bool {
        self.text().is_some() && self.until.is_none()
    }
    pub(super) fn clear(&mut self) {
        self.text = Clean::default();
        self.until = None;
    }
    pub(super) fn expire(&mut self) {
        if self.until.is_some_and(|until| Instant::now() >= until) {
            self.clear();
        }
    }
}
