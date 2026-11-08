use crate::tui::text;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone)]
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

pub(super) fn graphemes(row: &Row) -> impl Iterator<Item = (usize, usize, &str, Style)> {
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

fn selected(row: &Row, byte: usize, (start, last): (Point, Point)) -> bool {
    byte < row.byte + row.len && start <= (row.block, byte) && (row.block, byte) <= last
}

pub(super) fn highlight(row: &Row, range: (Point, Point)) -> Line<'_> {
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
    for (byte, _, grapheme, style) in graphemes(row) {
        let inside = selected(row, byte, range);
        line.spans.push(Span::styled(
            grapheme,
            if inside {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            },
        ));
    }
    line
}

pub(super) fn text<'a>(
    rows: impl IntoIterator<Item = &'a Row>,
    range: (Point, Point),
) -> Option<String> {
    let mut text = String::new();
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
        if previous_end.is_some_and(|(block, end)| row.block != block || row.byte > end) {
            text.push('\n');
        }
        for (byte, _, grapheme, _) in graphemes(row) {
            if selected(row, byte, range) {
                text.push_str(grapheme);
            }
        }
        previous_end = Some((row.block, row.byte + row.len));
    }
    (!text.is_empty()).then_some(text)
}

pub(super) fn wrap_line(line: Line<'static>, block: u64, byte: usize, width: usize) -> Vec<Row> {
    let mut start = 0;
    let mut newlines = line
        .spans
        .iter()
        .flat_map(|span| {
            let base = start;
            start += span.content.len();
            span.content
                .match_indices('\n')
                .map(move |(at, _)| base + at)
        })
        .peekable();
    let mut offset = 0;
    text::wrap(&line.spans, width.max(1), usize::MAX, text::Wrap::Prose)
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
                byte: byte + offset,
                len,
                indent,
                line: wrapped,
            };
            offset += len;
            if newlines.peek() == Some(&offset) {
                offset += 1;
                newlines.next();
            }
            row
        })
        .collect()
}
