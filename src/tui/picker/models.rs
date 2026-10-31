use crate::tui::text::Clean;
use crate::tui::widgets::menu::{Hint, Menu, Row, Section, row};
use crossterm::event::KeyEvent;

use super::{Choice, DEFAULT_EFFORT, Entry};
use crate::provider::{Model, Provider, Visibility};

enum Models {
    Loading,
    Ready(Vec<Model>),
    Failed(Clean),
}

enum Stage {
    Providers,
    Models { provider: Provider, models: Models },
    Efforts { provider: Provider, model: Model },
}

pub(in crate::tui) struct ModelPicker {
    entries: Vec<Entry>,
    stage: Stage,
    menu: Menu,
    cancellable: bool,
    hidden_models: bool,
}

impl ModelPicker {
    pub(super) fn providers(
        mut entries: Vec<Entry>,
        cancellable: bool,
        hidden_models: bool,
    ) -> Self {
        entries.sort_by_key(|entry| !entry.ready);
        let mut picker = Self {
            entries,
            stage: Stage::Providers,
            menu: Menu::default(),
            cancellable,
            hidden_models,
        };
        picker.refresh();
        picker
    }

    pub(super) fn efforts(provider: Provider, model: Model, current: Option<&str>) -> Self {
        let selected = current
            .and_then(|effort| model.efforts.iter().position(|known| known == effort))
            .map_or(0, |index| index + 1);
        let mut picker = Self {
            entries: Vec::new(),
            stage: Stage::Efforts { provider, model },
            menu: Menu::default(),
            cancellable: true,
            hidden_models: false,
        };
        picker.menu.selected = selected;
        picker.refresh();
        picker
    }

    pub(super) fn cancellable(&self) -> bool {
        self.cancellable
    }

    pub(super) fn models_loaded(&mut self, name: &str, result: Result<Vec<Model>, String>) -> bool {
        if let Stage::Models { provider, models } = &mut self.stage
            && provider.name() == name
            && matches!(models, Models::Loading)
        {
            *models = match result {
                Ok(list) => Models::Ready(
                    list.into_iter()
                        .filter(|model| {
                            self.hidden_models || model.visibility == Visibility::Listed
                        })
                        .collect(),
                ),
                Err(error) => Models::Failed(Clean::new(&error)),
            };
            self.menu.selected = 0;
            self.refresh();
            return true;
        }
        false
    }

    fn rows(&self) -> Vec<Row> {
        match &self.stage {
            Stage::Providers => self
                .entries
                .iter()
                .map(|entry| {
                    let note = if entry.ready {
                        entry
                            .provider
                            .endpoint()
                            .map_or_else(|| "claude -p provider".into(), ToString::to_string)
                    } else {
                        format!("not logged in · {}", entry.provider.login_hint())
                    };
                    Row {
                        label: Clean::new(entry.provider.label()),
                        ..row(entry.provider.name(), &note, entry.ready)
                    }
                })
                .collect(),
            Stage::Models {
                models: Models::Ready(list),
                ..
            } => list
                .iter()
                .map(|model| row(&model.id, model.name.as_deref().unwrap_or_default(), true))
                .collect(),
            Stage::Models {
                models: Models::Failed(_),
                ..
            } => vec![row("Retry", "", true)],
            Stage::Models { .. } => Vec::new(),
            Stage::Efforts { model, .. } => std::iter::once(DEFAULT_EFFORT)
                .chain(model.efforts.iter().map(String::as_str))
                .map(|effort| {
                    let default = model.default_effort.as_deref() == Some(effort);
                    row(effort, if default { "default" } else { "" }, true)
                })
                .collect(),
        }
    }

    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        self.menu.paste(text)
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Choice {
        let outcome = self.menu.key(key).into();
        match outcome {
            Choice::Back => {
                let choice = self.back();
                self.refresh();
                choice
            }
            Choice::Activate(value) => {
                let choice = self.choose(&value);
                self.refresh();
                choice
            }
            other => other,
        }
    }

    fn back(&mut self) -> Choice {
        self.menu.reset();
        match &self.stage {
            Stage::Models { .. } if !self.entries.is_empty() => self.stage = Stage::Providers,
            Stage::Efforts { provider, .. } if !self.entries.is_empty() => {
                let provider = provider.clone();
                self.stage = Stage::Models {
                    provider: provider.clone(),
                    models: Models::Loading,
                };
                return Choice::Load(provider);
            }
            _ if self.cancellable => return Choice::Cancelled,
            _ => {}
        }
        Choice::Back
    }

    fn choose(&mut self, value: &str) -> Choice {
        match &self.stage {
            Stage::Providers => {
                let Some(entry) = self
                    .entries
                    .iter()
                    .find(|entry| entry.provider.name() == value)
                else {
                    return Choice::Stay;
                };
                if !entry.ready {
                    return Choice::NeedsLogin(entry.provider.login_hint());
                }
                let provider = entry.provider.clone();
                self.menu.reset();
                self.stage = Stage::Models {
                    provider: provider.clone(),
                    models: Models::Loading,
                };
                Choice::Load(provider)
            }
            Stage::Models {
                provider,
                models: Models::Ready(list),
            } => {
                let Some(model) = list.iter().find(|model| model.id == value).cloned() else {
                    return Choice::Stay;
                };
                let provider = provider.clone();
                if model.efforts.is_empty() {
                    return Choice::Chosen(crate::agent::Settings {
                        provider,
                        model: model.id,
                        reasoning_effort: None,
                    });
                }
                self.menu.selected = default_effort(&model);
                self.stage = Stage::Efforts { provider, model };
                Choice::Stay
            }
            Stage::Models {
                provider,
                models: Models::Failed(_),
            } => {
                let provider = provider.clone();
                self.menu.selected = 0;
                self.stage = Stage::Models {
                    provider: provider.clone(),
                    models: Models::Loading,
                };
                Choice::Load(provider)
            }
            Stage::Models { .. } => Choice::Stay,
            Stage::Efforts { provider, model } => Choice::Chosen(crate::agent::Settings {
                provider: provider.clone(),
                model: model.id.clone(),
                reasoning_effort: (value != DEFAULT_EFFORT).then(|| value.to_owned()),
            }),
        }
    }

    fn title(&self) -> String {
        match &self.stage {
            Stage::Providers => "choose a provider".to_owned(),
            Stage::Models { provider, .. } => format!("{} › models", provider.label()),
            Stage::Efforts { provider, model } => format!(
                "{} › {} › choose a reasoning effort",
                provider.label(),
                model.id
            ),
        }
    }

    pub(super) fn menu(&self) -> &Menu {
        &self.menu
    }
    fn refresh(&mut self) {
        self.menu.title = self.title();
        self.menu.feedback = match &self.stage {
            Stage::Models {
                models: Models::Loading,
                ..
            } => "Loading models…".into(),
            Stage::Models {
                models: Models::Failed(error),
                ..
            } => error.to_string(),
            _ => String::new(),
        };
        self.menu.hints = match &self.stage {
            Stage::Models {
                models: Models::Failed(_),
                ..
            } => &[Hint::Rows("Enter retry"), Hint::Back("Esc back")],
            Stage::Models {
                models: Models::Loading,
                ..
            } => &[Hint::Back("Esc back")],
            Stage::Providers if !self.cancellable => &[
                Hint::Rows("Enter choose"),
                Hint::Rows("↑↓ move"),
                Hint::Always("Type filter"),
            ],
            _ => &[
                Hint::Rows("Enter choose"),
                Hint::Back("Esc back"),
                Hint::Rows("↑↓ move"),
                Hint::Always("Type filter"),
            ],
        };
        self.menu.set_sections(vec![Section {
            title: Clean::default(),
            rows: self.rows(),
        }]);
    }
}

fn default_effort(model: &Model) -> usize {
    model
        .default_effort
        .as_ref()
        .and_then(|effort| model.efforts.iter().position(|known| known == effort))
        .map_or(0, |index| index + 1)
}
