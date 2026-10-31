use crate::tui::{
    brand,
    text::{self, Clean, Item},
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Style, Stylize},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(in crate::tui) struct FrameStamp {
    pub(in crate::tui) seq: u64,
    pub(in crate::tui) size: (u16, u16),
}
pub(in crate::tui) enum Actions {
    Accept(&'static str),
    Approve,
}
pub(in crate::tui) enum Outcome {
    Stay,
    Accept,
    Cancel,
}
pub(in crate::tui) struct Confirm {
    pub(in crate::tui) title: String,
    pub(in crate::tui) review: ratatui::text::Text<'static>,
    pub(in crate::tui) actions: Actions,
    pub(in crate::tui) painted: Option<FrameStamp>,
}
impl Confirm {
    pub(in crate::tui) fn new(review: &Clean, actions: Actions) -> Self {
        Self {
            title: String::new(),
            review: review.to_string().into(),
            actions,
            painted: None,
        }
    }
    pub(in crate::tui) fn key(&self, key: KeyEvent, frame: Option<FrameStamp>) -> Outcome {
        match (&self.actions, key.code) {
            (_, KeyCode::Esc) | (Actions::Approve, KeyCode::Char('n' | 'd') | KeyCode::Enter) => {
                Outcome::Cancel
            }
            (Actions::Accept(_), KeyCode::Enter) | (Actions::Approve, KeyCode::Char('a' | 'y'))
                if key.modifiers.is_empty() && self.painted.is_some() && self.painted == frame =>
            {
                Outcome::Accept
            }
            _ => Outcome::Stay,
        }
    }
    pub(in crate::tui) fn panel(&self, area: Rect, feedback: bool) -> Option<Rect> {
        let width = area.width.saturating_sub(4);
        let rows = Paragraph::new(self.review.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        let height = rows + 4 + usize::from(feedback);
        (matches!(self.actions, Actions::Approve)
            && width >= 12
            && text::width(&self.title) + 4 <= usize::from(area.width)
            && rows > 0
            && height <= usize::from(area.height))
        .then(|| {
            Rect::new(
                area.x,
                area.bottom() - height as u16,
                area.width,
                height as u16,
            )
        })
    }
    pub(in crate::tui) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        feedback: Option<(&str, ratatui::style::Color)>,
    ) -> bool {
        let approval = matches!(self.actions, Actions::Approve);
        let (body, footer, full) = if approval {
            if let Some(panel) = self.panel(area, feedback.is_some()) {
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(brand()))
                    .title(format!(" {} · approval required ", self.title).bold())
                    .padding(Padding::horizontal(1));
                let inner = block.inner(panel);
                frame.render_widget(Clear, panel);
                frame.render_widget(block, panel);
                let footer_rows = 1 + u16::from(feedback.is_some());
                let body = Rect {
                    height: inner.height - footer_rows - 1,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new(format!(
                        "├{}┤",
                        "─".repeat(usize::from(panel.width.saturating_sub(2)))
                    ))
                    .fg(brand()),
                    Rect::new(panel.x, body.bottom(), panel.width, 1),
                );
                (
                    body,
                    Rect::new(
                        inner.x,
                        inner.bottom() - footer_rows,
                        inner.width,
                        footer_rows,
                    ),
                    true,
                )
            } else {
                frame.render_widget(Clear, area);
                (
                    Rect {
                        height: area.height.saturating_sub(1),
                        ..area
                    },
                    Rect::new(
                        area.x,
                        area.bottom().saturating_sub(1),
                        area.width,
                        area.height.min(1),
                    ),
                    false,
                )
            }
        } else {
            let full = area.width >= 12
                && area.height >= 3
                && Paragraph::new(self.review.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width)
                    <= usize::from(area.height - 2);
            (
                Rect {
                    height: area.height.saturating_sub(2),
                    ..area
                },
                Rect::new(
                    area.x,
                    area.bottom().saturating_sub(2),
                    area.width,
                    area.height.min(2),
                ),
                full,
            )
        };
        frame.render_widget(
            Paragraph::new(if full {
                self.review.clone()
            } else {
                "Resize to review".into()
            })
            .wrap(Wrap { trim: false }),
            body,
        );
        let hints = match (&self.actions, full) {
            (Actions::Accept(verb), true) => format!("Enter {verb} · Esc cancel"),
            (Actions::Approve, true) => "a approve · n/Enter/Esc deny".into(),
            (Actions::Accept(_), false) => "Esc cancel".into(),
            (Actions::Approve, false) => "n/Enter/Esc deny".into(),
        };
        let hint = text::fit(
            hints.split(" · ").enumerate().map(|(i, h)| Item {
                full: h.into(),
                short: None,
                priority: if approval { i } else { usize::MAX - i },
            }),
            usize::from(footer.width),
        );
        if footer.height > 1
            && let Some((value, color)) = feedback
        {
            frame.render_widget(
                Paragraph::new(text::clip_end(value, usize::from(footer.width))).fg(color),
                Rect {
                    height: 1,
                    ..footer
                },
            );
        }
        if footer.height > 0 {
            frame.render_widget(
                Paragraph::new(hint).dim(),
                Rect::new(footer.x, footer.bottom() - 1, footer.width, 1),
            );
        }
        full
    }
}
