use crate::tui::tasks::Ticket;
use crate::tui::widgets::confirm::FrameStamp;
use std::sync::Arc;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;

use crate::{
    mcp::Registry,
    preferences::{Change, Preferences},
    provider::{Model, Provider},
    record::Summary,
};

mod mcp;
mod models;
mod rewind;
mod sessions;
mod settings;

use mcp::McpPicker;
use models::ModelPicker;
use rewind::RewindPicker;
use sessions::SessionPicker;
use settings::SettingsPicker;

impl From<crate::tui::widgets::menu::Outcome> for Choice {
    fn from(value: crate::tui::widgets::menu::Outcome) -> Self {
        use crate::tui::widgets::menu::Outcome;
        match value {
            Outcome::Stay => Self::Stay,
            Outcome::Back => Self::Back,
            Outcome::Choose(key) => Self::Activate(key),
        }
    }
}

const DEFAULT_EFFORT: &str = "provider default";

pub(super) struct Entry {
    pub(super) provider: Provider,
    pub(super) ready: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Reviewer,
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Purpose {
    Resume,
    Delete,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Activate(String),
    Stay,
    Back,
    Details(String),
    Refused(String),
    Load(Provider),
    Chosen(crate::agent::Settings),
    Cancelled,
    NeedsLogin(String),
    Resume(String, std::path::PathBuf),
    Delete(String),
    Toggle(String),
    Rewind(usize),
    Prefer(Change),
    Route(Route),
}

pub(super) enum Picker {
    Models(Box<ModelPicker>),
    Settings(Box<SettingsPicker>),
    Sessions(SessionPicker),
    Rewind(RewindPicker),
    Mcp(McpPicker),
}

pub(super) fn content_area(area: Rect) -> Rect {
    let gutter = if area.width >= 14 { 2 } else { 0 };
    Rect::new(area.x + gutter, area.y, area.width - gutter, area.height)
}

impl Picker {
    pub(super) fn providers(entries: Vec<Entry>, cancellable: bool) -> Self {
        Self::Models(Box::new(ModelPicker::providers(
            entries,
            cancellable,
            false,
        )))
    }

    pub(super) fn efforts(provider: Provider, model: Model, current: Option<&str>) -> Self {
        Self::Models(Box::new(ModelPicker::efforts(provider, model, current)))
    }

    pub(super) fn settings(preferences: Preferences) -> Self {
        Self::Settings(Box::new(SettingsPicker::new(preferences)))
    }

    pub(super) fn window(preferences: Preferences) -> Self {
        Self::Settings(Box::new(SettingsPicker::window(preferences)))
    }

    pub(super) fn first_run(preferences: Preferences) -> Self {
        let mut picker = SettingsPicker::new(preferences);
        picker.first_run = true;
        picker.refresh();
        Self::Settings(Box::new(picker))
    }

    pub(super) fn sessions(purpose: Purpose, sessions: Vec<Summary>) -> Self {
        Self::Sessions(SessionPicker::new(purpose, sessions))
    }

    pub(super) fn messages(messages: Vec<(usize, Arc<str>)>) -> Self {
        Self::Rewind(RewindPicker::new(messages))
    }

    pub(super) fn mcps(registry: Arc<Registry>) -> Self {
        Self::Mcp(McpPicker::new(registry))
    }

    pub(super) fn route(&mut self, route: Route, entries: Vec<Entry>) {
        if let Self::Settings(settings) = self {
            settings.route(route, entries);
        }
    }

    pub(super) fn needs_context(&self) -> bool {
        matches!(self, Self::Settings(settings) if settings.needs_context())
    }

    pub(super) fn context_loaded(
        &mut self,
        provider: String,
        model: String,
        catalog: Option<Model>,
    ) {
        if let Self::Settings(settings) = self {
            settings.context_loaded(provider, model, catalog);
        }
    }

    pub(super) fn preference_accepted(&mut self, change: &Change, ticket: Ticket) {
        if let Self::Settings(settings) = self {
            settings.preference_accepted(change, ticket);
        }
    }

    pub(super) fn preference_failed(&mut self) {
        if let Self::Settings(settings) = self {
            settings.preference_failed();
        }
    }

    pub(super) fn preferences_saved(
        &mut self,
        preferences: &Preferences,
        ticket: Ticket,
    ) -> (bool, bool) {
        if let Self::Settings(settings) = self {
            settings.preferences_saved(preferences, ticket)
        } else {
            (false, false)
        }
    }

    pub(super) fn models_loaded(&mut self, name: &str, result: Result<Vec<Model>, String>) -> bool {
        let models = match self {
            Self::Models(models) => Some(&mut **models),
            Self::Settings(settings) => settings.routing(),
            _ => None,
        };
        models.is_some_and(|models| models.models_loaded(name, result))
    }

    pub(super) fn remove(&mut self, id: &str) -> bool {
        match self {
            Self::Sessions(sessions) => sessions.remove(id),
            _ => true,
        }
    }

    pub(super) fn cancellable(&self) -> bool {
        match self {
            Self::Models(models) => models.cancellable(),
            _ => true,
        }
    }

    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        match self {
            Self::Models(picker) => picker.paste(text),
            Self::Settings(picker) => picker.paste(text),
            Self::Sessions(picker) => picker.paste(text),
            Self::Rewind(picker) => picker.paste(text),
            Self::Mcp(picker) => picker.paste(text),
        }
    }

    pub(super) fn confirm(&self) -> Option<&crate::tui::widgets::confirm::Confirm> {
        match self {
            Self::Settings(p) => p.confirm(),
            Self::Sessions(p) => p.confirm.as_ref().map(|(_, confirm)| confirm),
            Self::Rewind(p) => p.confirm.as_ref().map(|(_, confirm)| confirm),
            _ => None,
        }
    }
    pub(super) fn confirm_mut(&mut self) -> Option<&mut crate::tui::widgets::confirm::Confirm> {
        match self {
            Self::Settings(p) => p.confirm_mut(),
            Self::Sessions(p) => p.confirm.as_mut().map(|(_, confirm)| confirm),
            Self::Rewind(p) => p.confirm.as_mut().map(|(_, confirm)| confirm),
            _ => None,
        }
    }
    pub(super) fn confirmation_active(&self) -> bool {
        self.confirm().is_some()
    }
    pub(super) fn registry_changed(&mut self) {
        if let Self::Mcp(p) = self {
            p.refresh();
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent, frame: Option<FrameStamp>) -> Choice {
        let choice = match self {
            Self::Models(picker) => picker.key(key),
            Self::Settings(picker) => picker.key(key, frame),
            Self::Sessions(picker) => picker.key(key, frame),
            Self::Rewind(picker) => picker.key(key, frame),
            Self::Mcp(picker) => picker.key(key),
        };
        if let Self::Settings(p) = self {
            p.refresh();
        }
        choice
    }

    pub(super) fn menu(&self) -> &crate::tui::widgets::menu::Menu {
        match self {
            Self::Models(p) => p.menu(),
            Self::Settings(p) => p.menu(),
            Self::Sessions(p) => p.menu(),
            Self::Rewind(p) => p.menu(),
            Self::Mcp(p) => p.menu(),
        }
    }
    pub(super) fn field(&self) -> Option<&crate::tui::widgets::field::Field> {
        if let Self::Settings(p) = self {
            p.field()
        } else {
            None
        }
    }
}
