use std::cell::Cell;

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

const RESIZE_HINT: &str = "Resize to review";

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
pub(in crate::tui) struct ReviewLayout {
    pub(in crate::tui) panel: Option<Rect>,
    rows: usize,
    reviewable: bool,
}
pub(in crate::tui) struct Confirm {
    pub(in crate::tui) title: String,
    pub(in crate::tui) review: ratatui::text::Text<'static>,
    pub(in crate::tui) actions: Actions,
    pub(in crate::tui) painted: Option<FrameStamp>,
    scroll: Cell<u16>,
    viewport: Cell<u16>,
    max_scroll: Cell<u16>,
}
impl Confirm {
    pub(in crate::tui) fn new(review: &Clean, actions: Actions) -> Self {
        Self {
            title: String::new(),
            review: review.to_string().into(),
            actions,
            painted: None,
            scroll: Cell::new(0),
            viewport: Cell::new(0),
            max_scroll: Cell::new(0),
        }
    }
    pub(in crate::tui) fn key(&self, key: KeyEvent, frame: Option<FrameStamp>) -> Outcome {
        if key.modifiers.is_empty() {
            let offset = self.scroll.get();
            let page = self.viewport.get().saturating_sub(1).max(1);
            let next = match key.code {
                KeyCode::Up => Some(offset.saturating_sub(1)),
                KeyCode::Down => Some(offset.saturating_add(1)),
                KeyCode::PageUp => Some(offset.saturating_sub(page)),
                KeyCode::PageDown => Some(offset.saturating_add(page)),
                KeyCode::Home => Some(0),
                KeyCode::End => Some(self.max_scroll.get()),
                _ => None,
            };
            if let Some(next) = next {
                self.scroll.set(next.min(self.max_scroll.get()));
                return Outcome::Stay;
            }
        }
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
    pub(in crate::tui) fn layout(&self, area: Rect, feedback: bool) -> ReviewLayout {
        let approval = matches!(self.actions, Actions::Approve);
        let overhead = 4 + usize::from(feedback);
        let inner = area.width.saturating_sub(4);
        let bordered = approval
            && inner >= 12
            && text::width(&self.title) + 4 <= usize::from(area.width)
            && usize::from(area.height) > overhead;
        let width = if bordered { inner } else { area.width };
        let reviewable = width >= 12 && if approval { bordered } else { area.height >= 3 };
        let rows = if reviewable {
            Paragraph::new(self.review.clone())
                .wrap(Wrap { trim: false })
                .line_count(width)
        } else {
            0
        };
        let panel = bordered.then(|| {
            let height = (rows + overhead)
                .min(usize::from(area.height / 2))
                .max(overhead + 1) as u16;
            Rect::new(area.x, area.bottom() - height, area.width, height)
        });
        ReviewLayout {
            panel,
            rows,
            reviewable,
        }
    }
    pub(in crate::tui) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        feedback: Option<(&str, ratatui::style::Color)>,
        layout: ReviewLayout,
    ) -> bool {
        let approval = matches!(self.actions, Actions::Approve);
        let (body, footer) = if approval {
            if let Some(panel) = layout.panel {
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
                )
            }
        } else {
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
            )
        };
        let review = Paragraph::new(if layout.reviewable {
            self.review.clone()
        } else {
            RESIZE_HINT.into()
        })
        .wrap(Wrap { trim: false });
        let max_scroll =
            u16::try_from(layout.rows.saturating_sub(usize::from(body.height))).unwrap_or(u16::MAX);
        self.viewport.set(body.height);
        self.max_scroll.set(max_scroll);
        self.scroll.set(self.scroll.get().min(max_scroll));
        frame.render_widget(review.scroll((self.scroll.get(), 0)), body);
        if footer.height == 0 {
            return layout.reviewable;
        }
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
        if !layout.reviewable && body.height == 0 {
            frame.render_widget(
                Paragraph::new(text::clip_end(RESIZE_HINT, usize::from(footer.width))).dim(),
                Rect::new(footer.x, footer.bottom() - 1, footer.width, 1),
            );
            return false;
        }
        let mut hints = Vec::new();
        if layout.reviewable {
            hints.push(Item {
                full: match self.actions {
                    Actions::Accept(verb) => format!("Enter {verb}"),
                    Actions::Approve => "a approve".into(),
                },
                short: None,
                priority: if approval { 1 } else { 2 },
            });
        }
        hints.push(Item {
            full: if approval {
                "n/Enter/Esc deny"
            } else {
                "Esc cancel"
            }
            .into(),
            short: None,
            priority: if approval { 2 } else { 1 },
        });
        if max_scroll > 0 {
            let first = usize::from(self.scroll.get()) + 1;
            let last = usize::from(self.scroll.get()) + usize::from(body.height);
            let position = format!("{first}–{last}/{}", layout.rows);
            hints.push(Item {
                full: format!("↑↓/PgUp/PgDn scroll {position}"),
                short: Some(format!("↑↓ {position}")),
                priority: 0,
            });
        }
        let hint = text::fit(hints, usize::from(footer.width));
        frame.render_widget(
            Paragraph::new(hint).dim(),
            Rect::new(footer.x, footer.bottom() - 1, footer.width, 1),
        );
        layout.reviewable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn approval_requires_a_fully_visible_title_and_review() {
        let title = "Python cell, after restarting the interpreter";
        for feedback in [false, true] {
            let mut confirm = Confirm::new(&Clean::new(&"pass\n".repeat(1000)), Actions::Approve);
            confirm.title = title.into();
            let min_width = text::width(title) as u16 + 4;
            let min_height = 5 + u16::from(feedback);
            for (seq, (width, height, allowed)) in [
                (80, 24, true),
                (min_width - 1, 24, false),
                (80, min_height - 1, false),
                (min_width, min_height, true),
            ]
            .into_iter()
            .enumerate()
            {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let stamp = FrameStamp {
                    seq: seq as u64,
                    size: (width, height),
                };
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        let reviewable = confirm.draw(
                            frame,
                            area,
                            feedback.then_some(("status", brand())),
                            confirm.layout(area, feedback),
                        );
                        assert_eq!(reviewable, allowed);
                        if reviewable {
                            confirm.painted = Some(stamp);
                        }
                    })
                    .unwrap();
                if allowed {
                    let rendered: String = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect();
                    assert!(rendered.contains(title));
                    assert!(rendered.contains("pass"));
                }
                assert_eq!(
                    matches!(
                        confirm.key(KeyCode::Char('a').into(), Some(stamp)),
                        Outcome::Accept
                    ),
                    allowed
                );
                assert!(matches!(
                    confirm.key(KeyCode::Esc.into(), Some(stamp)),
                    Outcome::Cancel
                ));
            }
        }
    }
}
