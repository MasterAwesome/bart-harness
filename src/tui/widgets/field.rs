use crate::{
    preferences::ContextWindow,
    tui::text::{self, Item},
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{Frame, layout::Rect, widgets::Paragraph};

#[derive(Default)]
pub(in crate::tui) struct Field {
    pub(in crate::tui) digits: String,
}
pub(in crate::tui) enum Outcome {
    Stay,
    Cancel,
    Save(Option<ContextWindow>),
    Invalid(String),
}
impl Field {
    pub(in crate::tui) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        let text = text.trim();
        let value = format!("{}{text}", self.digits);
        if text.is_empty()
            || value.len() > 20
            || !text.bytes().all(|b| b.is_ascii_digit())
            || value
                .parse::<u64>()
                .ok()
                .and_then(|n| ContextWindow::try_from(n).ok())
                .is_none()
        {
            return Err("paste refused: enter a valid context window");
        }
        self.digits = value;
        Ok(())
    }
    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Tab => return Outcome::Save(None),
            KeyCode::Backspace => {
                self.digits.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() && self.digits.len() < 20 => self.digits.push(c),
            KeyCode::Enter => {
                return if self.digits.is_empty() {
                    Outcome::Save(None)
                } else {
                    match self
                        .digits
                        .parse::<u64>()
                        .map_err(|_| "enter an integer token count".to_owned())
                        .and_then(ContextWindow::try_from)
                    {
                        Ok(n) => Outcome::Save(Some(n)),
                        Err(e) => Outcome::Invalid(e),
                    }
                };
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
        let hint = text::fit(
            ["Enter save", "Esc cancel", "Tab default"]
                .into_iter()
                .enumerate()
                .map(|(i, full)| Item {
                    full: full.into(),
                    short: None,
                    priority: usize::MAX - i,
                }),
            width,
        );
        let field = text::clip_start(&format!("{}▏", self.digits), width);
        let mut lines = match area.height {
            0 => return,
            1 | 2 => vec![field],
            3 => vec![text::clip_end("Context window (tokens)", width), field],
            _ => vec![
                text::clip_end("Context window (tokens)", width),
                text::clip_end(feedback.map_or("", |(text, _)| text), width),
                field,
            ],
        };
        while lines.len() < usize::from(area.height.saturating_sub(1)) {
            lines.push(String::new());
        }
        if area.height > 1 {
            lines.push(hint);
        }
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let line = ratatui::text::Line::from(line);
                        if i == 1
                            && area.height >= 4
                            && let Some((_, color)) = feedback
                        {
                            line.style(ratatui::style::Style::new().fg(color))
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>(),
            ),
            area,
        );
    }
}
