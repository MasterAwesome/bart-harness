use crate::tui::brand;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

pub(super) fn draw_composer(
    frame: &mut Frame,
    composer: Rect,
    rows: &[(Line<'static>, usize)],
    (row, column): (usize, usize),
    lead: u16,
    decorated: bool,
    name: Option<&str>,
) {
    let visible = usize::from(composer.height.saturating_sub(u16::from(decorated) * 2));
    let first = (row + 1).saturating_sub(visible);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(index, (text, _))| {
            let mut spans = if index == 0 {
                vec![Span::styled(
                    "❯ ",
                    Style::new().fg(brand()).add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::raw(" ".repeat(usize::from(lead)))]
            };
            spans.extend(text.spans.clone());
            Line::from(spans)
        })
        .collect();
    let paragraph = Paragraph::new(lines);
    let paragraph = if decorated {
        paragraph.block(
            Block::new()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_set(symbols::border::LIGHT_DOUBLE_DASHED)
                .border_style(Style::new().dim()),
        )
    } else {
        paragraph
    };
    frame.render_widget(paragraph, composer);
    if decorated
        && let Some(name) = name
        && composer.width >= 4
    {
        let name = crate::tui::text::clip_end(name, usize::from(composer.width - 4));
        let title = Line::from(Span::styled(format!(" {name} ╌"), Style::new().dim()));
        let area = Rect {
            height: 1,
            ..composer
        };
        frame.render_widget(Paragraph::new(title).right_aligned(), area);
    }
    if visible == 0 || composer.width == 0 {
        return;
    }
    let x = (composer.x + lead + u16::try_from(column).unwrap_or(0))
        .min(composer.right().saturating_sub(1));
    let y = composer.y + u16::from(decorated) + u16::try_from(row - first).unwrap_or(0);
    frame.set_cursor_position((x, y));
}
