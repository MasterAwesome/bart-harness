use crate::tui::tasks::Ticket;
use crate::tui::text::Clean;
use crate::tui::widgets::confirm::FrameStamp;
use crate::tui::widgets::{
    confirm::{Actions, Confirm, Outcome},
    field::{Field, Outcome as FieldOutcome},
    menu::{Hint, Menu, Row, Section, State as RowState, row},
};
use std::fmt::Write as _;

use crossterm::event::{KeyCode, KeyEvent};

use super::{Choice, DEFAULT_EFFORT, Entry, Route, models::ModelPicker};
use crate::{
    approval::Mode,
    preferences::{Change, ContextWindow, Preferences},
    provider::Model,
    tool::Network,
};

const CONTEXT: &str = "Context window";
const SEARCH: &str = "Web search";

struct Target {
    provider: String,
    model: String,
    catalog: Option<Model>,
}

enum Interaction {
    Browsing,
    Confirming(Change, Confirm),
    Editing(Editor),
    Routing(Route, Box<ModelPicker>),
}

enum Editor {
    Draft(Field),
    Saving { field: Field, ticket: Ticket },
    Changed(Field),
}
impl Editor {
    fn field(&self) -> &Field {
        match self {
            Self::Draft(field) | Self::Saving { field, .. } | Self::Changed(field) => field,
        }
    }
    fn digits(&self) -> &str {
        &self.field().digits
    }
    fn change(&mut self, digits: String) {
        let field = Field { digits };
        *self = match self {
            Self::Draft(_) => Self::Draft(field),
            Self::Saving { .. } | Self::Changed(_) => Self::Changed(field),
        };
    }
}
pub(in crate::tui) struct SettingsPicker {
    preferences: Preferences,
    mode: Interaction,
    menu: Menu,
    context: Option<Target>,
    prompting: bool,
    pub(super) first_run: bool,
}

impl SettingsPicker {
    pub(super) fn new(preferences: Preferences) -> Self {
        let mut picker = Self {
            preferences,
            mode: Interaction::Browsing,
            menu: Menu::default(),
            context: None,
            prompting: false,
            first_run: false,
        };
        picker.refresh();
        picker
    }

    pub(super) fn window(preferences: Preferences) -> Self {
        let mut picker = Self::new(preferences);
        picker.menu.filter = CONTEXT.into();
        picker.refresh();
        picker.prompting = true;
        picker
    }

    pub(super) fn route(&mut self, route: Route, entries: Vec<Entry>) {
        self.mode =
            Interaction::Routing(route, Box::new(ModelPicker::providers(entries, true, true)));
    }

    pub(super) fn routing(&mut self) -> Option<&mut ModelPicker> {
        match &mut self.mode {
            Interaction::Routing(_, picker) => Some(&mut **picker),
            _ => None,
        }
    }

    pub(super) fn needs_context(&self) -> bool {
        !matches!(self.mode, Interaction::Routing(..)) && self.context.is_none()
    }

    pub(super) fn context_loaded(
        &mut self,
        provider: String,
        model: String,
        catalog: Option<Model>,
    ) {
        self.context = Some(Target {
            provider,
            model,
            catalog,
        });
        self.refresh();
        if self.prompting
            && matches!(self.mode, Interaction::Browsing)
            && self.menu.selected_key() == Some(CONTEXT)
        {
            self.mode = Interaction::Editing(Editor::Draft(Field::default()));
        }
    }

    pub(super) fn preference_accepted(&mut self, change: &Change, ticket: Ticket) {
        if matches!(change, Change::ContextWindow { .. })
            && let Interaction::Editing(editor) = &mut self.mode
        {
            *editor = Editor::Saving {
                field: Field {
                    digits: editor.digits().into(),
                },
                ticket,
            };
        }
    }
    pub(super) fn preference_failed(&mut self) {
        if let Interaction::Editing(editor) = &mut self.mode {
            *editor = Editor::Draft(Field {
                digits: editor.digits().into(),
            });
        }
    }
    pub(super) fn preferences_saved(
        &mut self,
        preferences: &Preferences,
        ticket: Ticket,
    ) -> (bool, bool) {
        self.preferences.clone_from(preferences);
        let owned = matches!(&self.mode, Interaction::Editing(Editor::Saving { ticket: pending, .. }) if *pending == ticket);
        if owned {
            self.mode = Interaction::Browsing;
        } else if let Interaction::Editing(editor) = &mut self.mode {
            *editor = Editor::Draft(Field {
                digits: editor.digits().into(),
            });
        }
        self.refresh();
        (
            owned && self.prompting,
            matches!(self.mode, Interaction::Editing(_)),
        )
    }

    fn sections(&self) -> Vec<Section> {
        let mut sections = settings_sections(&self.preferences);
        sections
            .last_mut()
            .expect("settings section")
            .rows
            .push(self.context_row());
        sections
    }

    fn context_row(&self) -> Row {
        let Some(Target {
            provider,
            model,
            catalog,
        }) = &self.context
        else {
            return row(CONTEXT, "loading catalog…", false);
        };
        let preferences = &self.preferences;
        if let Err(error) = preferences.window(provider, model, catalog.as_ref()) {
            return row(CONTEXT, &error, true);
        }
        let current = preferences
            .context_override(provider, model)
            .map(ContextWindow::get);
        let default = catalog.as_ref().and_then(|m| m.context_window);
        let maximum = maximum(catalog.as_ref());
        let custom = current.filter(|&value| Some(value) != maximum);
        let editing = matches!(self.mode, Interaction::Editing(_));
        let mut row = row(CONTEXT, "", true);
        row.options.push((
            Clean::new(&default.map_or_else(
                || "unknown".to_owned(),
                |n| format!("{} default", tokens(n)),
            )),
            !editing && current.is_none(),
        ));
        if let Some(max) = maximum {
            row.options.push((
                Clean::new(&format!("{} max", tokens(max))),
                !editing && current == Some(max),
            ));
        }
        row.options.push(match &self.mode {
            Interaction::Editing(editor) => (Clean::new(&format!("{}▏", editor.digits())), true),
            _ => (
                Clean::new(
                    &custom
                        .map_or_else(|| "custom".to_owned(), |n| format!("{} custom", tokens(n))),
                ),
                custom.is_some(),
            ),
        });
        row
    }

    pub(super) fn key(&mut self, key: KeyEvent, frame: Option<FrameStamp>) -> Choice {
        if let Interaction::Routing(route, picker) = &mut self.mode {
            let route = *route;
            return match picker.key(key) {
                Choice::Cancelled => {
                    self.mode = Interaction::Browsing;
                    Choice::Back
                }
                Choice::Chosen(choice) => {
                    self.mode = Interaction::Browsing;
                    let chosen = choice.choice();
                    self.propose(match route {
                        Route::Reviewer => Change::Reviewer(chosen),
                        Route::Search => Change::Search(Some(chosen)),
                    })
                }
                step => step,
            };
        }
        if matches!(self.mode, Interaction::Editing(_)) {
            return self.edit_context(key);
        }
        if let Interaction::Confirming(change, confirm) = &self.mode {
            return match confirm.key(key, frame) {
                Outcome::Accept => {
                    let change = change.clone();
                    self.mode = Interaction::Browsing;
                    Choice::Prefer(change)
                }
                Outcome::Cancel => {
                    self.mode = Interaction::Browsing;
                    Choice::Back
                }
                _ => Choice::Stay,
            };
        }
        if self.menu.selected_key() == Some("Tool view") {
            match key.code {
                KeyCode::Left => return Choice::Prefer(Change::Ui(crate::preferences::Ui::Cells)),
                KeyCode::Right => {
                    return Choice::Prefer(Change::Ui(crate::preferences::Ui::ToolsOnly));
                }
                _ => {}
            }
        }
        if self.menu.selected_key() == Some(SEARCH) {
            match (key.code, self.preferences.search.is_some()) {
                (KeyCode::Left, false) => return Choice::Stay,
                (KeyCode::Left | KeyCode::Tab, true) => {
                    return Choice::Prefer(Change::Search(None));
                }
                (KeyCode::Right | KeyCode::Enter, _) | (KeyCode::Tab, false) => {
                    return Choice::Route(Route::Search);
                }
                _ => {}
            }
        }
        if self.menu.selected_key() == Some(CONTEXT) && self.context.is_some() {
            if matches!(key.code, KeyCode::Tab | KeyCode::Right) {
                return self.next_context_choice(false);
            }
            if key.code == KeyCode::Left {
                return self.next_context_choice(true);
            }
            if let KeyCode::Char(c) = key.code
                && c.is_ascii_digit()
            {
                self.mode = Interaction::Editing(Editor::Draft(Field {
                    digits: c.to_string(),
                }));
                return Choice::Stay;
            }
        }
        if key.code == KeyCode::Left {
            if self.menu.selected_key() == Some("Approval") {
                let approval = &self.preferences.permissions.approval;
                return match approval.mode() {
                    Mode::Auto => self.propose(Change::Approval(Mode::Manual)),
                    Mode::Manual => self.propose(Change::Approval(Mode::None)),
                    Mode::None => match approval.reviewer() {
                        Some(reviewer) => self.propose(Change::Reviewer(reviewer.clone())),
                        None => Choice::Route(Route::Reviewer),
                    },
                };
            }
            return self
                .menu
                .selected_key()
                .map(str::to_owned)
                .map_or(Choice::Stay, |value| {
                    if value == CONTEXT {
                        Choice::Stay
                    } else {
                        self.choose(&value)
                    }
                });
        }
        match self.menu.key(key).into() {
            Choice::Back => Choice::Cancelled,
            Choice::Activate(value) => self.choose(&value),
            other => other,
        }
    }

    fn choose(&mut self, value: &str) -> Choice {
        if value == CONTEXT {
            if self.context.is_some() {
                self.mode = Interaction::Editing(Editor::Draft(Field::default()));
            }
            return Choice::Stay;
        }
        let approval = &self.preferences.permissions.approval;
        if value == "Reviewer"
            || value == "Approval"
                && approval.mode().next() == Mode::Auto
                && approval.reviewer().is_none()
        {
            return Choice::Route(Route::Reviewer);
        }
        let change = setting_change(&self.preferences, value);
        match change {
            Some(change) => self.propose(change),
            None => Choice::Stay,
        }
    }

    fn propose(&mut self, change: Change) -> Choice {
        let mut next = self.preferences.clone();
        if change.apply(&mut next).is_ok()
            && next.permissions.confinement() != self.preferences.permissions.confinement()
        {
            match review_text(&change) {
                Some(review) => {
                    self.mode = Interaction::Confirming(
                        change,
                        Confirm::new(&review, Actions::Accept("restart")),
                    );
                    Choice::Stay
                }
                None => {
                    Choice::Refused("cannot display this settings change for confirmation".into())
                }
            }
        } else {
            Choice::Prefer(change)
        }
    }

    fn next_context_choice(&mut self, backwards: bool) -> Choice {
        let Target {
            provider,
            model,
            catalog,
        } = self.context.as_ref().expect("catalog loaded");
        let current = self
            .preferences
            .context_override(provider, model)
            .map(ContextWindow::get);
        let maximum = maximum(catalog.as_ref());
        if backwards {
            return match (current, maximum) {
                (None, _) => {
                    self.mode = Interaction::Editing(Editor::Draft(Field::default()));
                    Choice::Stay
                }
                (Some(value), Some(max)) if value != max => match ContextWindow::try_from(max) {
                    Ok(max) => self.context_change(Some(max)),
                    Err(error) => Choice::Refused(error),
                },
                _ => self.context_change(None),
            };
        }
        match (current, maximum) {
            (None, Some(maximum)) => match ContextWindow::try_from(maximum) {
                Ok(maximum) => self.context_change(Some(maximum)),
                Err(error) => Choice::Refused(error),
            },
            (Some(value), maximum) if Some(value) != maximum => self.context_change(None),
            _ => {
                self.mode = Interaction::Editing(Editor::Draft(Field::default()));
                Choice::Stay
            }
        }
    }

    fn context_change(&self, tokens: Option<ContextWindow>) -> Choice {
        let Target {
            provider, model, ..
        } = self.context.as_ref().expect("catalog loaded");
        Choice::Prefer(Change::ContextWindow {
            provider: provider.clone(),
            model: model.clone(),
            tokens,
        })
    }

    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        match &mut self.mode {
            Interaction::Browsing => self.menu.paste(text),
            Interaction::Routing(_, picker) => picker.paste(text),
            Interaction::Confirming(..) => Err("paste refused during confirmation"),
            Interaction::Editing(editor) => {
                let mut field = Field {
                    digits: editor.digits().into(),
                };
                field.paste(text)?;
                editor.change(field.digits);
                Ok(())
            }
        }
    }
    fn edit_context(&mut self, key: KeyEvent) -> Choice {
        let Interaction::Editing(editor) = &mut self.mode else {
            return Choice::Stay;
        };
        let mut field = Field {
            digits: editor.digits().into(),
        };
        let outcome = field.key(key);
        if field.digits != editor.digits() {
            editor.change(field.digits);
        }
        match outcome {
            FieldOutcome::Cancel => {
                self.mode = Interaction::Browsing;
                Choice::Stay
            }
            FieldOutcome::Save(None) if self.prompting && key.code == KeyCode::Enter => {
                Choice::Refused("enter the model's context window in tokens".into())
            }
            FieldOutcome::Save(tokens) => self.context_change(tokens),
            FieldOutcome::Invalid(error) => Choice::Refused(error),
            FieldOutcome::Stay => Choice::Stay,
        }
    }

    pub(super) fn confirm(&self) -> Option<&Confirm> {
        match &self.mode {
            Interaction::Confirming(_, confirm) => Some(confirm),
            _ => None,
        }
    }
    pub(super) fn confirm_mut(&mut self) -> Option<&mut Confirm> {
        match &mut self.mode {
            Interaction::Confirming(_, confirm) => Some(confirm),
            _ => None,
        }
    }
    pub(super) fn menu(&self) -> &Menu {
        match &self.mode {
            Interaction::Routing(_, picker) => picker.menu(),
            _ => &self.menu,
        }
    }
    pub(super) fn field(&self) -> Option<&Field> {
        match &self.mode {
            Interaction::Editing(editor) => Some(editor.field()),
            _ => None,
        }
    }
    pub(super) fn refresh(&mut self) {
        self.menu.title = if self.first_run {
            "first run: check these settings"
        } else {
            "settings"
        }
        .into();
        self.menu.set_sections(self.sections());
        let ready = self.menu.selected_key().is_some_and(|key| {
            self.menu
                .sections
                .iter()
                .flat_map(|s| &s.rows)
                .any(|r| r.key == key && r.state == RowState::Ready)
        });
        self.menu.hints = match self.menu.selected_key() {
            _ if !ready => &[
                Hint::Back("Esc back"),
                Hint::Rows("↑↓ move"),
                Hint::Always("Type filter"),
            ],
            Some(CONTEXT) => &[
                Hint::Rows("Enter edit"),
                Hint::Back("Esc back"),
                Hint::Rows("←→ switch"),
                Hint::Rows("digits custom"),
                Hint::Rows("↑↓ move"),
            ],
            Some(SEARCH) => &[
                Hint::Rows("Enter route"),
                Hint::Back("Esc back"),
                Hint::Rows("← off"),
                Hint::Rows("Tab toggle"),
                Hint::Rows("↑↓ move"),
            ],
            _ => &[
                Hint::Rows("Enter/←→ change"),
                Hint::Back("Esc back"),
                Hint::Rows("↑↓ move"),
                Hint::Always("Type filter"),
            ],
        };
    }
}

fn maximum(catalog: Option<&Model>) -> Option<u64> {
    catalog.and_then(|m| {
        m.max_context_window
            .filter(|&max| Some(max) != m.context_window)
    })
}

fn tokens(n: u64) -> String {
    if n.is_multiple_of(1000) {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.into()
}

fn setting_change(preferences: &Preferences, key: &str) -> Option<Change> {
    let permissions = &preferences.permissions;
    match key {
        "Approval" => match (
            permissions.approval.mode().next(),
            permissions.approval.reviewer(),
        ) {
            (Mode::Auto, Some(reviewer)) => Some(Change::Reviewer(reviewer.clone())),
            (Mode::Auto, None) => None,
            (mode, _) => Some(Change::Approval(mode)),
        },
        "Sandbox" if cfg!(target_os = "linux") => {
            Some(Change::Sandbox(!permissions.sandbox.enabled))
        }
        "Network" if cfg!(target_os = "linux") && permissions.sandbox.enabled => {
            Some(Change::Network(match permissions.sandbox.network {
                Network::On => Network::Off,
                Network::Off => Network::On,
            }))
        }
        "Diffs" => Some(Change::Diffs(!preferences.diffs)),
        "Wait for quota resets" => Some(Change::WaitForReset(!preferences.wait_for_reset)),
        "Reasoning text" => Some(Change::ReasoningSummaries(!preferences.reasoning_summaries)),
        "Tool view" => Some(Change::Ui(match preferences.ui {
            crate::preferences::Ui::Cells => crate::preferences::Ui::ToolsOnly,
            crate::preferences::Ui::ToolsOnly => crate::preferences::Ui::Cells,
        })),
        _ => None,
    }
}

fn settings_sections(preferences: &Preferences) -> Vec<Section> {
    let permissions = &preferences.permissions;
    let setting = |key, state: String| {
        let mut row = row(
            key,
            "",
            setting_change(preferences, key).is_some() || matches!(key, "Reviewer" | "Approval"),
        );
        row.options.push((Clean::new(&state), true));
        row
    };
    let mut permissions_rows = vec![setting(
        "Approval",
        permissions.approval.mode().name().into(),
    )];
    if let Some(reviewer) = permissions
        .approval
        .reviewer()
        .filter(|_| permissions.approval.mode() == Mode::Auto)
    {
        permissions_rows.push(setting(
            "Reviewer",
            format!(
                "{} · {} · {}",
                reviewer.provider,
                reviewer.model,
                reviewer.effort.as_deref().unwrap_or(DEFAULT_EFFORT)
            ),
        ));
    }
    permissions_rows.extend([
        setting("Sandbox", on_off(permissions.sandbox.enabled)),
        setting(
            "Network",
            if permissions.sandbox.enabled {
                on_off(permissions.sandbox.network == Network::On)
            } else {
                "on · sandbox off".into()
            },
        ),
    ]);
    let mut other_rows = vec![
        setting("Diffs", on_off(preferences.diffs)),
        setting("Wait for quota resets", on_off(preferences.wait_for_reset)),
        setting("Reasoning text", on_off(preferences.reasoning_summaries)),
    ];
    let mut ui = row("Tool view", "", true);
    ui.options = vec![
        (
            Clean::literal("cells"),
            preferences.ui == crate::preferences::Ui::Cells,
        ),
        (
            Clean::literal("tools-only"),
            preferences.ui == crate::preferences::Ui::ToolsOnly,
        ),
    ];
    other_rows.push(ui);
    let mut search = row(SEARCH, "", true);
    let route = preferences.search.as_ref().map_or_else(
        || "on".to_owned(),
        |route| {
            let mut label = format!("{}/{}", route.provider, route.model);
            if let Some(effort) = &route.effort {
                let _ = write!(label, " ({effort})");
            }
            label
        },
    );
    search.options = vec![
        (Clean::literal("off"), preferences.search.is_none()),
        (Clean::new(&route), preferences.search.is_some()),
    ];
    other_rows.push(search);
    vec![
        Section {
            title: Clean::literal("Permissions"),
            rows: permissions_rows,
        },
        Section {
            title: Clean::literal("Other settings"),
            rows: other_rows,
        },
    ]
}

fn review_text(change: &Change) -> Option<Clean> {
    let target = match change {
        Change::Sandbox(enabled) => format!("Sandbox: {}", on_off(*enabled)),
        Change::Network(network) => format!("Network: {}", on_off(*network == Network::On)),
        Change::Approval(mode) => format!("Approval: {}", mode.name()),
        Change::Reviewer(choice) => format!(
            "Reviewer: {}/{}{}",
            choice.provider,
            choice.model,
            choice
                .effort
                .as_ref()
                .map_or(String::new(), |effort| format!(" ({effort})"))
        ),
        _ => return None,
    };
    Some(Clean::new(&format!(
        "{target}\nPython state lost.\nBackground jobs stop."
    )))
}
