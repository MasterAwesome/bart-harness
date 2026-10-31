use crate::tui::text::{self, Clean, Item};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::Stylize,
    text::{Line, Span},
    widgets::Paragraph,
};

#[derive(PartialEq, Eq)]
pub(in crate::tui) enum State {
    Ready,
    Unavailable,
}
pub(in crate::tui) struct Row {
    pub(in crate::tui) key: String,
    pub(in crate::tui) label: Clean,
    pub(in crate::tui) note: Clean,
    pub(in crate::tui) options: Vec<(Clean, bool)>,
    pub(in crate::tui) state: State,
}
pub(in crate::tui) fn row(key: &str, note: &str, ready: bool) -> Row {
    Row {
        key: key.into(),
        label: Clean::new(key),
        note: Clean::new(note),
        options: Vec::new(),
        state: if ready {
            State::Ready
        } else {
            State::Unavailable
        },
    }
}
pub(in crate::tui) struct Section {
    pub(in crate::tui) title: Clean,
    pub(in crate::tui) rows: Vec<Row>,
}
pub(in crate::tui) enum Hint {
    Back(&'static str),
    Rows(&'static str),
    Always(&'static str),
}
#[derive(Default)]
pub(in crate::tui) struct Menu {
    pub(in crate::tui) title: String,
    pub(in crate::tui) sections: Vec<Section>,
    pub(in crate::tui) filter: String,
    pub(in crate::tui) feedback: String,
    pub(in crate::tui) hints: &'static [Hint],
    pub(in crate::tui) selected: usize,
    pub(in crate::tui) clip_note_start: bool,
    visible: Vec<(usize, Option<usize>)>,
    label_width: usize,
}
pub(in crate::tui) enum Outcome {
    Stay,
    Back,
    Choose(String),
}
impl Menu {
    pub(in crate::tui) fn set_sections(&mut self, sections: Vec<Section>) {
        self.sections = sections;
        self.label_width = self
            .sections
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| text::width(&r.label))
            .max()
            .unwrap_or(0);
        self.refilter();
    }
    fn refilter(&mut self) {
        self.visible.clear();
        let filter = self.filter.to_lowercase();
        for (s, section) in self.sections.iter().enumerate() {
            let start = self.visible.len();
            if !section.title.is_empty() {
                self.visible.push((s, None));
            }
            for (r, row) in section.rows.iter().enumerate() {
                if filter.is_empty()
                    || format!("{} {} {}", row.key, row.label, row.note)
                        .to_lowercase()
                        .contains(&filter)
                {
                    self.visible.push((s, Some(r)));
                }
            }
            if self.visible.len() == start + 1 && !section.title.is_empty() {
                self.visible.pop();
            }
        }
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        if self
            .visible
            .get(self.selected)
            .is_some_and(|(_, r)| r.is_none())
        {
            self.step(1);
        }
    }
    fn selected_row(&self) -> Option<&Row> {
        let (s, r) = *self.visible.get(self.selected)?;
        Some(&self.sections[s].rows[r?])
    }
    pub(in crate::tui) fn selected_key(&self) -> Option<&str> {
        self.selected_row().map(|r| r.key.as_str())
    }
    fn step(&mut self, delta: isize) {
        let mut next = self.selected;
        for _ in 0..delta.unsigned_abs() {
            let candidate = next.saturating_add_signed(delta.signum());
            if candidate >= self.visible.len() || candidate == next {
                break;
            }
            next = candidate;
            while self.visible[next].1.is_none() {
                let candidate = next.saturating_add_signed(delta.signum());
                if candidate >= self.visible.len() || candidate == next {
                    return;
                }
                next = candidate;
            }
            self.selected = next;
        }
    }
    pub(in crate::tui) fn reset(&mut self) {
        self.filter.clear();
        self.selected = 0;
        self.refilter();
    }
    pub(in crate::tui) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        if !crate::tui::paste::accepts(text, self.filter.len(), 256) {
            return Err("paste refused: filter requires one line within 256 bytes");
        }
        self.filter.push_str(text);
        self.selected = 0;
        self.refilter();
        Ok(())
    }
    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || control => self.step(-1),
            KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || control => {
                self.step(1)
            }
            KeyCode::PageUp => self.step(-8),
            KeyCode::PageDown => self.step(8),
            KeyCode::Esc if !self.filter.is_empty() => self.reset(),
            KeyCode::Esc | KeyCode::Left => return Outcome::Back,
            KeyCode::Backspace if self.filter.is_empty() => return Outcome::Back,
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
                self.refilter();
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Tab => {
                if let Some(key) = self.selected_key() {
                    return Outcome::Choose(key.into());
                }
            }
            KeyCode::Char(c)
                if !control
                    && !crate::terminal_text::unsafe_character(c)
                    && self.filter.len() + c.len_utf8() <= 256 =>
            {
                self.filter.push(c);
                self.selected = 0;
                self.refilter();
            }
            _ => {}
        }
        Outcome::Stay
    }
    pub(in crate::tui) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        feedback: Option<(&str, ratatui::style::Color)>,
    ) {
        let width = usize::from(area.width);
        let height = usize::from(area.height);
        if width == 0 || height == 0 {
            return;
        }
        let title = if self.filter.is_empty() {
            self.title.clone()
        } else {
            format!("{} · filter: {}", self.title, self.filter)
        };
        let mut lines = vec![Line::from(text::clip_end(&title, width)).bold()];
        let (value, color) = feedback.map_or((self.feedback.as_str(), None), |(value, color)| {
            (value, Some(color))
        });
        let span = Span::raw(value.to_owned());
        let span = if let Some(color) = color {
            span.fg(color)
        } else {
            span
        };
        let limit = height.saturating_sub(2).min(3);
        let (feedback, _) = text::wrap(&[span], width, limit, text::Wrap::Prose);
        let feedback_rows = if value.is_empty() {
            limit.min(1)
        } else {
            feedback.len()
        };
        for _ in 0..feedback_rows {
            lines.push(Line::default());
        }
        let room = height.saturating_sub(lines.len() + 1);
        let first = self.selected.saturating_sub(room.saturating_sub(1));
        let label_width = self
            .label_width
            .min(width.saturating_sub(10).min(width / 2));
        let indent = if self.sections.iter().any(|s| !s.title.is_empty()) {
            "  "
        } else {
            ""
        };
        for (i, (s, r)) in self.visible.iter().enumerate().skip(first).take(room) {
            let section = &self.sections[*s];
            let Some(r) = r else {
                lines.push(
                    Line::from(text::clip_end(&section.title, width)).fg(crate::tui::brand()),
                );
                continue;
            };
            let row = &section.rows[*r];
            let selected = i == self.selected;
            let label_width = if row.options.is_empty() && row.note.is_empty() {
                width.saturating_sub(6)
            } else {
                label_width
            };
            let label = text::clip_end(&row.label, label_width);
            let prefix = format!(
                "{indent}{}{}{}  ",
                if selected { "❯ " } else { "  " },
                label,
                " ".repeat(label_width.saturating_sub(text::width(&label)))
            );
            let value = if row.options.is_empty() {
                row.note.to_string()
            } else {
                row.options
                    .iter()
                    .map(|(v, active)| {
                        if *active {
                            format!("[{v}]")
                        } else {
                            v.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            let room = width.saturating_sub(text::width(&prefix));
            let value = if text::width(&value) > room {
                row.options
                    .iter()
                    .find(|(_, active)| *active)
                    .map_or(value, |(v, _)| {
                        format!("[{}]", text::clip_end(v, room.saturating_sub(2)))
                    })
            } else {
                value
            };
            let value = if self.clip_note_start {
                text::clip_start(&value, room)
            } else {
                text::clip_end(&value, room)
            };
            let line = Line::from(format!("{prefix}{value}"));
            lines.push(if row.state == State::Unavailable {
                line.dim()
            } else if selected {
                line.bold()
            } else {
                line
            });
        }
        if self.visible.is_empty()
            && room > 0
            && (!self.filter.is_empty() || self.feedback.is_empty())
        {
            lines.push(Line::from(if self.filter.is_empty() {
                "  No items"
            } else {
                "  No matches"
            }));
        }
        while lines.len() < height.saturating_sub(1) {
            lines.push(Line::default());
        }
        let selected = self.selected_row().is_some();
        let hints = self.hints.iter().filter_map(|hint| match hint {
            Hint::Rows(label) => selected.then_some(*label),
            Hint::Back(label) => Some(if self.filter.is_empty() {
                *label
            } else {
                "Esc clear"
            }),
            Hint::Always(label) => Some(*label),
        });
        let hint = text::fit(
            hints.enumerate().map(|(i, h)| Item {
                full: h.into(),
                short: None,
                priority: usize::MAX - i,
            }),
            width,
        );
        if height > 1 {
            lines.push(Line::from(hint).dim());
        }
        frame.render_widget(Paragraph::new(lines), area);
        frame.render_widget(
            Paragraph::new(
                feedback
                    .into_iter()
                    .map(|(line, _)| line)
                    .collect::<Vec<_>>(),
            ),
            Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                feedback_rows as u16,
            ),
        );
    }
}
