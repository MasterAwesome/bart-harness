use super::{Choice, Purpose};
use crate::tui::widgets::confirm::FrameStamp;
use crate::{
    record::Summary,
    tui::{
        text::Clean,
        widgets::{
            confirm::{Actions, Confirm, Outcome},
            menu::{Hint, Menu, Section, row},
        },
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, SystemTime};

pub(in crate::tui) struct SessionPicker {
    purpose: Purpose,
    sessions: Vec<Summary>,
    all: bool,
    menu: Menu,
    pub(super) confirm: Option<(String, Confirm)>,
}
impl SessionPicker {
    pub(super) fn new(purpose: Purpose, sessions: Vec<Summary>) -> Self {
        let mut picker = Self {
            purpose,
            sessions,
            all: false,
            menu: Menu::default(),
            confirm: None,
        };
        picker.refresh();
        picker
    }
    fn refresh(&mut self) {
        self.menu.clip_note_start = self.all;
        self.menu.title = format!(
            "{} a session · {}",
            if self.purpose == Purpose::Resume {
                "resume"
            } else {
                "delete"
            },
            if self.all {
                "all directories"
            } else {
                "this directory"
            }
        );
        self.menu.hints = &[
            Hint::Rows("Enter choose"),
            Hint::Back("Esc close"),
            Hint::Always("Tab scope"),
            Hint::Rows("↑↓ move"),
            Hint::Always("Type filter"),
        ];
        self.menu.set_sections(vec![Section {
            title: Default::default(),
            rows: self
                .sessions
                .iter()
                .filter(|s| self.all || s.here)
                .map(|session| {
                    let (preview, note) = session.description.as_ref().map_or_else(
                        |_| (session.id.as_str(), "unreadable · Ctrl+O".into()),
                        |d| {
                            (
                                if d.preview.is_empty() {
                                    "(no messages)"
                                } else {
                                    &d.preview
                                },
                                if self.all {
                                    d.meta.cwd.display().to_string()
                                } else {
                                    format!("{} · {}", d.meta.provider, d.meta.model)
                                },
                            )
                        },
                    );
                    let mut r = row(
                        &session.id,
                        &format!(
                            "{note}{}",
                            if session.in_use {
                                " · open or inaccessible"
                            } else {
                                ""
                            }
                        ),
                        !session.in_use
                            && (self.purpose == Purpose::Delete || session.description.is_ok()),
                    );
                    r.label = Clean::new(&format!("{}  {preview}", age(session.modified)));
                    r
                })
                .collect(),
        }]);
    }
    pub(super) fn remove(&mut self, id: &str) -> bool {
        self.sessions.retain(|s| s.id != id);
        self.refresh();
        !self.sessions.is_empty()
    }
    pub(super) fn menu(&self) -> &Menu {
        &self.menu
    }
    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        if self.confirm.is_some() {
            Err("paste refused during confirmation")
        } else {
            self.menu.paste(text)
        }
    }
    pub(super) fn key(&mut self, key: KeyEvent, frame: Option<FrameStamp>) -> Choice {
        if let Some((_, confirm)) = &self.confirm {
            return match confirm.key(key, frame) {
                Outcome::Accept => {
                    let (target, _) = self.confirm.take().expect("confirmation active");
                    Choice::Delete(target)
                }
                Outcome::Cancel => {
                    self.confirm = None;
                    Choice::Stay
                }
                _ => Choice::Stay,
            };
        }
        if key.code == KeyCode::Tab {
            self.all = !self.all;
            self.menu.selected = 0;
            self.refresh();
            return Choice::Stay;
        }
        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self
                .sessions
                .iter()
                .find(|s| Some(s.id.as_str()) == self.menu.selected_key())
                .and_then(|s| s.description.as_ref().err())
                .map_or(Choice::Stay, |e| Choice::Details(e.clone()));
        }
        match self.menu.key(key).into() {
            Choice::Back => Choice::Cancelled,
            Choice::Activate(id) => {
                let session = self
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .expect("menu session");
                if session.in_use {
                    return Choice::Stay;
                }
                if self.purpose == Purpose::Resume {
                    return session
                        .description
                        .as_ref()
                        .map_or(Choice::Stay, |d| Choice::Resume(id, d.meta.cwd.clone()));
                }
                let (preview, directory) = session.description.as_ref().map_or_else(
                    |_| {
                        (
                            "unreadable (Ctrl+O for details)".into(),
                            "unreadable".into(),
                        )
                    },
                    |d| {
                        (
                            crate::tui::text::clip_end(&d.preview, 80),
                            d.meta.cwd.display().to_string(),
                        )
                    },
                );
                let confirm = Confirm::new(
                    &Clean::new(&format!(
                        "Delete session ID: {id}\nDirectory: {directory}\nPreview: {preview}\nSession record and output are permanently deleted. External files and effects are not undone."
                    )),
                    Actions::Accept("delete"),
                );
                self.confirm = Some((id, confirm));
                Choice::Stay
            }
            other => other,
        }
    }
}
fn age(then: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(then)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}
