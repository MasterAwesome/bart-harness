use crate::tui::text;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation as _;

pub(super) struct Row {
    pub(super) block: u64,
    pub(super) byte: usize,
    pub(super) len: usize,
    pub(super) indent: usize,
    pub(super) line: Line<'static>,
}
pub(super) type Point = (u64, usize);

fn pad_spans(line: &Line<'_>, indent: usize) -> usize {
    let mut cells = 0;
    line.spans
        .iter()
        .take_while(|span| {
            let pad = cells < indent;
            cells += text::width(&span.content);
            pad
        })
        .count()
}

fn graphemes(row: &Row) -> impl Iterator<Item = (usize, usize, &str, Style)> {
    let mut byte = row.byte;
    let mut column = row.indent;
    row.line
        .spans
        .iter()
        .skip(pad_spans(&row.line, row.indent))
        .flat_map(move |span| {
            span.content
                .graphemes(true)
                .map(move |grapheme| (grapheme, span.style))
        })
        .map(move |(grapheme, style)| {
            let at = (byte, column, grapheme, style);
            byte += grapheme.len();
            column += text::width(grapheme);
            at
        })
}

pub(super) fn point_at(row: &Row, column: usize) -> Point {
    let end = row.byte + row.len;
    let byte = graphemes(row)
        .take_while(|(byte, ..)| *byte < end)
        .take_while(|(_, first, ..)| *first <= column)
        .last()
        .map_or(row.byte, |(byte, ..)| byte);
    (row.block, byte)
}

pub(super) fn borrowed<'a>(line: &'a Line<'_>) -> Line<'a> {
    Line {
        spans: line
            .spans
            .iter()
            .map(|span| Span::styled(span.content.as_ref(), span.style))
            .collect(),
        style: line.style,
        alignment: line.alignment,
    }
}

pub(super) fn columns(row: &Row, selection: Option<(Point, Point)>) -> (Line<'_>, String) {
    let end = row.byte + row.len;
    let mut line = Line {
        spans: row
            .line
            .spans
            .iter()
            .take(pad_spans(&row.line, row.indent))
            .map(|span| Span::styled(span.content.as_ref(), span.style))
            .collect(),
        style: row.line.style,
        alignment: row.line.alignment,
    };
    let mut text = String::new();
    for (byte, _, grapheme, style) in graphemes(row) {
        let inside = byte < end
            && selection.is_some_and(|(start, last)| {
                start <= (row.block, byte) && (row.block, byte) <= last
            });
        line.spans.push(Span::styled(
            grapheme,
            if inside {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            },
        ));
        if inside {
            text.push_str(grapheme);
        }
    }
    (line, text)
}

pub(super) fn text<'a>(
    rows: impl IntoIterator<Item = &'a Row>,
    range: (Point, Point),
) -> Option<String> {
    let mut lines: Vec<(u64, String)> = Vec::new();
    let mut previous_end = None;
    for row in rows {
        if row.block < range.0.0 || row.block > range.1.0 {
            continue;
        }
        if row.block == range.0.0
            && row.byte + row.len <= range.0.1
            && (row.len != 0 || row.byte < range.0.1)
            || row.block == range.1.0 && row.byte > range.1.1
        {
            continue;
        }
        let piece = columns(row, Some(range)).1;
        match lines.last_mut() {
            Some((id, text)) if *id == row.block => {
                if previous_end.is_some_and(|end| row.byte > end) {
                    text.push('\n');
                }
                text.push_str(&piece);
            }
            _ => lines.push((row.block, piece)),
        }
        previous_end = Some(row.byte + row.len);
    }
    let text = lines
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

pub(super) fn wrap_line(line: Line<'static>, block: u64, byte: usize, width: u16) -> Vec<Row> {
    let mut byte = byte;
    text::wrap(
        &line.spans,
        usize::from(width.max(1)),
        usize::MAX,
        text::Wrap::Prose,
    )
    .0
    .into_iter()
    .map(|(mut wrapped, indent)| {
        wrapped.style = line.style;
        let len = wrapped
            .spans
            .iter()
            .skip(pad_spans(&wrapped, indent))
            .map(|span| span.content.len())
            .sum();
        let row = Row {
            block,
            byte,
            len,
            indent,
            line: wrapped,
        };
        byte += len;
        row
    })
    .collect()
}
