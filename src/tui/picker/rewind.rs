use super::Choice;
use crate::tui::widgets::confirm::FrameStamp;
use crate::tui::{
    text::{self, Clean},
    widgets::{
        confirm::{Actions, Confirm, Outcome},
        menu::{Hint, Menu, Section, row},
    },
};
use crossterm::event::KeyEvent;
use std::sync::Arc;
pub(in crate::tui) struct RewindPicker {
    menu: Menu,
    pub(super) confirm: Option<(usize, Confirm)>,
}
impl RewindPicker {
    pub(super) fn new(messages: Vec<(usize, Arc<str>)>) -> Self {
        let mut menu = Menu::default();
        menu.title = "Rewind targets".into();
        menu.hints = &[
            Hint::Rows("Enter review"),
            Hint::Back("Esc close"),
            Hint::Rows("↑↓ move"),
            Hint::Always("Type filter"),
        ];
        menu.set_sections(vec![Section {
            title: Default::default(),
            rows: messages
                .into_iter()
                .rev()
                .map(|(at, text)| {
                    let mut r = row(&at.to_string(), "", true);
                    r.label = Clean::new(&text.split_whitespace().collect::<Vec<_>>().join(" "));
                    r
                })
                .collect(),
        }]);
        Self {
            menu,
            confirm: None,
        }
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
                    Choice::Rewind(target)
                }
                Outcome::Cancel => {
                    self.confirm = None;
                    Choice::Stay
                }
                _ => Choice::Stay,
            };
        }
        match self.menu.key(key).into() {
            Choice::Back => Choice::Cancelled,
            Choice::Activate(at) => {
                let row = self
                    .menu
                    .sections
                    .iter()
                    .flat_map(|s| &s.rows)
                    .find(|r| r.key == at)
                    .expect("rewind row");
                self.confirm = Some((
                    at.parse().expect("rewind target"),
                    Confirm::new(
                        &Clean::new(&format!(
                            "Preview: {}\nThe selected message and all later conversation will be removed. Python state will be discarded. Background jobs will be stopped. External files and effects are not undone.",
                            text::clip_end(&row.label, 80)
                        )),
                        Actions::Accept("rewind"),
                    ),
                ));
                Choice::Stay
            }
            other => other,
        }
    }
}
