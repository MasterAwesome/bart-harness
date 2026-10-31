use super::Choice;
use crate::{
    mcp::{Registry, State},
    tui::widgets::menu::{Hint, Menu, Section, row},
};
use crossterm::event::{KeyCode, KeyEvent};
use std::sync::Arc;
pub(in crate::tui) struct McpPicker {
    registry: Arc<Registry>,
    server: Option<String>,
    menu: Menu,
}
impl McpPicker {
    pub(super) fn new(registry: Arc<Registry>) -> Self {
        let mut picker = Self {
            registry,
            server: None,
            menu: Menu::default(),
        };
        picker.refresh();
        picker
    }
    pub(super) fn refresh(&mut self) {
        self.menu.title = self.server.clone().unwrap_or_else(|| "MCP servers".into());
        self.menu.hints = if self.server.is_none() {
            &[
                Hint::Rows("Enter view"),
                Hint::Back("Esc close"),
                Hint::Rows("Space toggle"),
                Hint::Rows("↑↓ move"),
                Hint::Always("Type filter"),
            ]
        } else {
            &[
                Hint::Back("Esc back"),
                Hint::Rows("↑↓ move"),
                Hint::Always("Type filter"),
            ]
        };
        self.menu.feedback.clear();
        let rows = if let Some(server) = &self.server {
            if let Some((_, State::Failed(error))) = self
                .registry
                .states()
                .iter()
                .find(|(name, _)| name == server)
            {
                self.menu.feedback = error.lines().next().unwrap_or_default().into();
                error
                    .lines()
                    .skip(1)
                    .map(|line| row(line, "", false))
                    .collect()
            } else {
                self.registry.catalog(server).map_or_else(Vec::new, |c| {
                    c.tools()
                        .map(|(name, about)| row(name, about, true))
                        .collect()
                })
            }
        } else {
            if self.registry.is_empty() {
                self.menu.feedback = "No MCP servers configured · bart mcp add".into();
            }
            self.registry
                .states()
                .into_iter()
                .map(|(name, state)| {
                    let (note, ready) = match state {
                        State::Starting => ("starting…".into(), false),
                        State::Ready { about, tool_count } => {
                            (format!("ready · {tool_count} tools · {about}"), true)
                        }
                        State::Failed(error) => (
                            format!(
                                "unavailable · {} · Enter shows why",
                                error.lines().next().unwrap_or_default()
                            ),
                            false,
                        ),
                        State::Disabled(reason) => (format!("disabled · {reason}"), false),
                    };
                    row(&name, &note, ready)
                })
                .collect()
        };
        self.menu.set_sections(vec![Section {
            title: Default::default(),
            rows,
        }]);
    }
    pub(super) fn menu(&self) -> &Menu {
        &self.menu
    }
    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        self.menu.paste(text)
    }
    pub(super) fn key(&mut self, key: KeyEvent) -> Choice {
        if key.code == KeyCode::Char(' ') && self.server.is_none() {
            return self
                .menu
                .selected_key()
                .map_or(Choice::Stay, |s| Choice::Toggle(s.into()));
        }
        match self.menu.key(key).into() {
            Choice::Back if self.server.is_some() => {
                self.server = None;
                self.menu.reset();
                self.refresh();
                Choice::Back
            }
            Choice::Back => Choice::Cancelled,
            Choice::Activate(name) => {
                if self.server.is_none() {
                    if let Some((_, State::Failed(error))) = self
                        .registry
                        .states()
                        .iter()
                        .find(|(server, _)| server == &name)
                    {
                        return Choice::Details(format!("MCP server {name}\n{error}"));
                    }
                    self.server = Some(name);
                    self.menu.reset();
                    self.refresh();
                }
                Choice::Stay
            }
            other => other,
        }
    }
}
