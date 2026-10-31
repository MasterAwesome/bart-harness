use crate::tui::text::Clean;
use crate::tui::text::Wrap::{Prose, Table};
mod math;

use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use super::text;
use pulldown_cmark::{Alignment, BrokenLink, CodeBlockKind, Event, LinkType, Options, Parser, Tag};
use ratatui::{
    style::Style,
    text::{Line, Span, Text},
};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum RenderRow {
    Literal(Line<'static>),
    Table(Box<TableRow>),
    TableClipped,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TableRow {
    header: Arc<TableHeader>,
    cells: Vec<Vec<Span<'static>>>,
    kind: TableKind,
    prefix: Vec<Span<'static>>,
}

#[derive(Clone, Debug, PartialEq)]
struct TableHeader {
    cells: Vec<Vec<Span<'static>>>,
    alignments: Vec<Alignment>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TableKind {
    Header,
    Body,
    Footer,
}

impl RenderRow {
    pub(super) fn source(&self) -> String {
        match self {
            Self::Literal(line) => line.to_string(),
            Self::Table(row) => row
                .cells
                .iter()
                .map(|cell| {
                    cell.iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" | "),
            Self::TableClipped => "table clipped: display limit exceeded".into(),
        }
    }
    pub(super) fn project(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Self::Literal(line) => vec![line.clone()],
            Self::TableClipped => vec![table_notice(
                "table clipped: display limit exceeded",
                usize::from(width.max(1)),
            )],
            Self::Table(row) => {
                if self.bytes() > MAX_TABLE_ROW_BYTES {
                    return vec![table_notice(
                        "table clipped: display limit exceeded",
                        usize::from(width.max(1)),
                    )];
                }
                row.project(usize::from(width.max(1)))
            }
        }
    }
    pub(super) fn bytes(&self) -> usize {
        fn spans(spans: &[Span<'static>]) -> usize {
            size_of_val(spans)
                + spans
                    .iter()
                    .map(|s| match &s.content {
                        Cow::Borrowed(text) => text.len(),
                        Cow::Owned(text) => text.capacity(),
                    })
                    .sum::<usize>()
        }
        match self {
            Self::Literal(line) => {
                size_of_val(line)
                    + spans(&line.spans)
                    + line.spans.capacity().saturating_sub(line.spans.len())
                        * size_of::<Span<'static>>()
            }
            Self::TableClipped => 0,
            Self::Table(row) => {
                size_of_val(row.as_ref())
                    + size_of::<TableHeader>()
                    + row.cells.capacity().saturating_sub(row.cells.len())
                        * size_of::<Vec<Span<'static>>>()
                    + row
                        .header
                        .cells
                        .capacity()
                        .saturating_sub(row.header.cells.len())
                        * size_of::<Vec<Span<'static>>>()
                    + spans(&row.prefix)
                    + row.prefix.capacity().saturating_sub(row.prefix.len())
                        * size_of::<Span<'static>>()
                    + row
                        .cells
                        .iter()
                        .map(|cell| {
                            size_of_val(cell)
                                + spans(cell)
                                + cell.capacity().saturating_sub(cell.len())
                                    * size_of::<Span<'static>>()
                        })
                        .sum::<usize>()
                    + row
                        .header
                        .cells
                        .iter()
                        .map(|cell| {
                            size_of_val(cell)
                                + spans(cell)
                                + cell.capacity().saturating_sub(cell.len())
                                    * size_of::<Span<'static>>()
                        })
                        .sum::<usize>()
                    + row.header.alignments.capacity() * size_of::<Alignment>()
            }
        }
    }
    pub(super) fn is_table(&self) -> bool {
        matches!(self, Self::Table(_) | Self::TableClipped)
    }
    pub(super) fn bound_table(&mut self, bytes: usize) -> bool {
        if matches!(self, Self::Table(_)) && bytes > MAX_TABLE_ROW_BYTES {
            *self = Self::TableClipped;
            true
        } else {
            false
        }
    }
    pub(super) fn prefix(&mut self, prefix: Span<'static>) {
        match self {
            Self::Literal(line) => line.spans.insert(0, prefix),
            Self::Table(row) => row.prefix.insert(0, prefix),
            Self::TableClipped => {}
        }
    }
    pub(super) fn is_blank(&self) -> bool {
        matches!(self, Self::Literal(line) if line.spans.iter().all(|span| span.content.trim().is_empty()))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct RenderText {
    pub(super) lines: Vec<RenderRow>,
}

impl From<Text<'static>> for RenderText {
    fn from(text: Text<'static>) -> Self {
        Self {
            lines: text
                .lines
                .into_iter()
                .map(|mut line| {
                    line.style = text.style.patch(line.style);
                    RenderRow::Literal(line)
                })
                .collect(),
        }
    }
}

impl From<Vec<RenderRow>> for RenderText {
    fn from(lines: Vec<RenderRow>) -> Self {
        Self { lines }
    }
}

fn literals(lines: impl IntoIterator<Item = Line<'static>>) -> Vec<RenderRow> {
    lines.into_iter().map(RenderRow::Literal).collect()
}

struct Reference {
    url: String,
    title: String,
}
type References = HashMap<unicase::UniCase<String>, Reference>;

const MAX_TABLE_COLUMNS: usize = 120;
const MAX_TABLE_ROW_BYTES: usize = 64 * 1024;
const MAX_TABLE_RENDERED_ROWS: usize = 32 * 1024;

const OPTIONS: Options = Options::ENABLE_STRIKETHROUGH
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_TABLES)
    .union(Options::ENABLE_MATH)
    .union(Options::ENABLE_FOOTNOTES);

enum Node<'a> {
    Element(Tag<'a>, Vec<Self>),
    Leaf(Event<'a>),
}

fn nodes<'a>(events: &mut impl Iterator<Item = Event<'a>>, depth: usize) -> Option<Vec<Node<'a>>> {
    let mut out = Vec::new();
    while let Some(event) = events.next() {
        match event {
            Event::Start(tag) => {
                if depth == 32 {
                    return None;
                }
                out.push(Node::Element(tag, nodes(events, depth + 1)?));
            }
            Event::End(_) => break,
            event => out.push(Node::Leaf(event)),
        }
    }
    Some(out)
}

pub(super) fn render_detail(markdown: &str, width: u16) -> Text<'static> {
    Text::from(
        render_filtered(markdown, u16::MAX, &References::new())
            .iter()
            .flat_map(|row| row.project(width))
            .collect::<Vec<_>>(),
    )
}

pub(super) fn contains_table(markdown: &str) -> bool {
    Parser::new_ext(markdown, OPTIONS).any(|event| matches!(event, Event::Start(Tag::Table(_))))
}

fn render_filtered(markdown: &str, width: u16, references: &References) -> Vec<RenderRow> {
    let (normalized, _) = latex_delimiters(markdown);
    render_events(markdown, width, parser(&normalized, markdown, references))
}

fn render_events<'a>(
    markdown: &str,
    width: u16,
    mut events: impl Iterator<Item = Event<'a>>,
) -> Vec<RenderRow> {
    let Some(nodes) = nodes(&mut events, 0) else {
        return literals(
            text::wrap(
                &[Span::raw(markdown.to_owned())],
                usize::from(width).max(1),
                usize::MAX,
                Prose,
            )
            .0
            .into_iter()
            .map(|(line, _)| line),
        );
    };
    blocks(&nodes, usize::from(width).max(1))
}

fn inline(nodes: &[Node<'_>], style: Style, out: &mut Vec<Span<'static>>) {
    for node in nodes {
        match node {
            Node::Element(tag, children) => {
                let next = match tag {
                    Tag::Emphasis => style.italic(),
                    Tag::Strong => style.bold(),
                    Tag::Strikethrough => style.crossed_out(),
                    Tag::Link { .. } => style.underlined(),
                    _ => style,
                };
                let start = out.len();
                inline(children, next, out);
                if let Tag::Link { dest_url, .. } = tag {
                    let label: String = out[start..]
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect();
                    if label != dest_url.as_ref() {
                        out.push(Span::styled(Clean::new(&format!(" ({dest_url})")), style));
                    }
                }
            }
            Node::Leaf(event) => match event {
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    out.push(Span::styled(Clean::new(text), style));
                }
                Event::InlineMath(text) => out.push(Span::styled(
                    math::present(text).into_owned(),
                    style.fg(super::theme().code),
                )),
                Event::Code(text) => out.push(Span::styled(
                    Clean::new(text),
                    style.fg(super::theme().code),
                )),
                Event::DisplayMath(text) => out.push(Span::styled(
                    format!("\n{}\n", math::present(text.trim_matches('\n'))),
                    style.fg(super::theme().code),
                )),
                Event::FootnoteReference(label) => out.push(Span::styled(
                    Clean::new(&format!("[^{label}]")),
                    style.fg(super::theme().code),
                )),
                Event::SoftBreak | Event::HardBreak => out.push(Span::styled("\n", style)),
                Event::TaskListMarker(done) => {
                    out.push(Span::styled(if *done { "[x] " } else { "[ ] " }, style));
                }
                _ => {}
            },
        }
    }
}

fn prefixed(
    lines: Vec<Line<'static>>,
    first: &str,
    rest: &str,
    style: Style,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            line.spans.insert(
                0,
                Span::styled(
                    if index == 0 {
                        first.to_owned()
                    } else {
                        rest.to_owned()
                    },
                    style,
                ),
            );
            line
        })
        .collect()
}

fn prefixed_rows(
    mut lines: Vec<RenderRow>,
    first: &str,
    rest: &str,
    style: Style,
) -> Vec<RenderRow> {
    for (index, line) in lines.iter_mut().enumerate() {
        line.prefix(Span::styled(
            if index == 0 {
                first.to_owned()
            } else {
                rest.to_owned()
            },
            style,
        ));
    }
    lines
}

fn blocks(nodes: &[Node<'_>], width: usize) -> Vec<RenderRow> {
    block_lines(nodes, width, false)
}

fn block_lines(nodes: &[Node<'_>], width: usize, compact: bool) -> Vec<RenderRow> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for node in nodes {
        if matches!(
            node,
            Node::Element(
                Tag::Emphasis
                    | Tag::Strong
                    | Tag::Strikethrough
                    | Tag::Superscript
                    | Tag::Subscript
                    | Tag::Link { .. }
                    | Tag::Image { .. },
                _
            )
        ) {
            inline(std::slice::from_ref(node), Style::new(), &mut pending);
            continue;
        }
        let Node::Element(tag, children) = node else {
            if matches!(node, Node::Leaf(Event::Rule)) {
                if !pending.is_empty() {
                    out.extend(literals(
                        text::wrap(&std::mem::take(&mut pending), width, usize::MAX, Prose)
                            .0
                            .into_iter()
                            .map(|(line, _)| line),
                    ));
                }
                if !out.is_empty() {
                    out.push(RenderRow::Literal(Line::default()));
                }
                out.push(RenderRow::Literal(
                    Line::from("─".repeat(width.min(3))).style(Style::new().dim()),
                ));
            } else {
                inline(std::slice::from_ref(node), Style::new(), &mut pending);
            }
            continue;
        };
        let tight = !pending.is_empty() && matches!(tag, Tag::List(_));
        if !pending.is_empty() {
            out.extend(literals(
                text::wrap(&std::mem::take(&mut pending), width, usize::MAX, Prose)
                    .0
                    .into_iter()
                    .map(|(line, _)| line),
            ));
        }
        let lines = match tag {
            Tag::Paragraph | Tag::Heading { .. } | Tag::HtmlBlock => {
                literals(prose(tag, children, width))
            }
            Tag::CodeBlock(kind) => literals(code(children, kind, width)),
            Tag::BlockQuote(_) => prefixed_rows(
                blocks(children, width.saturating_sub(2).max(1)),
                "│ ",
                "│ ",
                super::transcript::muted(Style::new()),
            ),
            Tag::List(start) => {
                let mut number = start.unwrap_or(1);
                let mut lines = Vec::new();
                for child in children {
                    if let Node::Element(Tag::Item, body) = child {
                        let marker = if start.is_some() {
                            format!("{number}. ")
                        } else {
                            "- ".to_owned()
                        };
                        let indent = " ".repeat(text::width(&marker));
                        lines.extend(prefixed_rows(
                            block_lines(
                                body,
                                width.saturating_sub(text::width(&marker)).max(1),
                                true,
                            ),
                            &marker,
                            &indent,
                            Style::new(),
                        ));
                        number += 1;
                    }
                }
                lines
            }
            Tag::FootnoteDefinition(label) => {
                let marker = Clean::new(&format!("[^{label}]: ")).to_string();
                let indent = " ".repeat(text::width(&marker));
                prefixed_rows(
                    blocks(children, width.saturating_sub(text::width(&marker)).max(1)),
                    &marker,
                    &indent,
                    Style::new(),
                )
            }
            Tag::Table(alignments) => table(children, alignments),
            _ => blocks(children, width),
        };
        if !out.is_empty() && !tight && !compact && !matches!(tag, Tag::Item) {
            out.push(RenderRow::Literal(Line::default()));
        }
        out.extend(lines);
    }
    if !pending.is_empty() {
        out.extend(literals(
            text::wrap(&pending, width, usize::MAX, Prose)
                .0
                .into_iter()
                .map(|(line, _)| line),
        ));
    }
    out
}

fn prose(tag: &Tag<'_>, children: &[Node<'_>], width: usize) -> Vec<Line<'static>> {
    let style = match tag {
        Tag::Heading {
            level: pulldown_cmark::HeadingLevel::H1,
            ..
        } => Style::new().bold().underlined(),
        Tag::Heading { .. } => Style::new().bold(),
        _ => Style::new(),
    };
    let mut spans = Vec::new();
    let mut lines = Vec::new();
    for child in children {
        if let Node::Leaf(Event::DisplayMath(text)) = child {
            if !spans.is_empty() {
                lines.extend(
                    text::wrap(&std::mem::take(&mut spans), width, usize::MAX, Prose)
                        .0
                        .into_iter()
                        .map(|(line, _)| line),
                );
            }
            let style = Style::new().fg(super::theme().code);
            lines.extend(prefixed(
                text::wrap(
                    &[Span::styled(
                        math::present(text.trim_matches('\n')).into_owned(),
                        style,
                    )],
                    width.saturating_sub(2).max(1),
                    usize::MAX,
                    Prose,
                )
                .0
                .into_iter()
                .map(|(line, _)| line)
                .collect::<Vec<_>>(),
                "  ",
                "  ",
                style,
            ));
        } else {
            inline(std::slice::from_ref(child), style, &mut spans);
        }
    }
    if !spans.is_empty() || lines.is_empty() {
        lines.extend(
            text::wrap(&spans, width, usize::MAX, Prose)
                .0
                .into_iter()
                .map(|(line, _)| line),
        );
    }
    if matches!(tag, Tag::HtmlBlock) && lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    lines
}

fn code(children: &[Node<'_>], kind: &CodeBlockKind<'_>, width: usize) -> Vec<Line<'static>> {
    let mut spans = Vec::new();
    inline(children, Style::new(), &mut spans);
    let code: String = spans.iter().map(|span| span.content.as_ref()).collect();
    let code = code.strip_suffix('\n').unwrap_or(&code);
    code_lines(code, kind, width)
}

fn code_lines(code: &str, kind: &CodeBlockKind<'_>, width: usize) -> Vec<Line<'static>> {
    let language = match kind {
        CodeBlockKind::Fenced(lang) => lang.split_whitespace().next(),
        CodeBlockKind::Indented => None,
    };
    let lines = language
        .and_then(|language| super::highlight::highlight(code, language))
        .unwrap_or_else(|| {
            code.split('\n')
                .map(|line| Line::from(line.to_owned()).style(Style::new().fg(super::theme().code)))
                .collect()
        });
    lines
        .into_iter()
        .flat_map(|line| {
            let spans: Vec<_> = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content, line.style.patch(span.style)))
                .collect();
            text::wrap(&spans, width, usize::MAX, Prose)
                .0
                .into_iter()
                .map(|(line, _)| line)
        })
        .collect()
}

fn table_cell(
    spans: &[Span<'static>],
    header: bool,
    width: usize,
    limit: usize,
) -> (Vec<Line<'static>>, bool) {
    let spans: Vec<_> = spans
        .iter()
        .map(|span| {
            Span::styled(
                span.content.clone(),
                if header {
                    span.style.bold()
                } else {
                    span.style
                },
            )
        })
        .collect();
    let (rows, clipped) = text::wrap(&spans, width, limit, Table);
    (rows.into_iter().map(|(line, _)| line).collect(), clipped)
}

fn table_notice(text: &str, width: usize) -> Line<'static> {
    Line::styled(super::text::clip_end(text, width), Style::new().dim())
}

fn table(nodes: &[Node<'_>], alignments: &[Alignment]) -> Vec<RenderRow> {
    let mut rows: Vec<Vec<Vec<Span<'static>>>> = nodes
        .iter()
        .filter_map(|node| {
            let Node::Element(_, cells) = node else {
                return None;
            };
            Some(
                cells
                    .iter()
                    .filter_map(|cell| {
                        let Node::Element(Tag::TableCell, body) = cell else {
                            return None;
                        };
                        let mut spans = Vec::new();
                        inline(body, Style::new(), &mut spans);
                        Some(spans)
                    })
                    .collect(),
            )
        })
        .collect();
    if rows.is_empty() || alignments.is_empty() {
        return Vec::new();
    }
    let header = Arc::new(TableHeader {
        cells: rows[0].clone(),
        alignments: alignments.to_vec(),
    });
    let mut out: Vec<_> = rows
        .drain(..)
        .enumerate()
        .map(|(index, cells)| {
            RenderRow::Table(Box::new(TableRow {
                header: Arc::clone(&header),
                cells,
                kind: if index == 0 {
                    TableKind::Header
                } else {
                    TableKind::Body
                },
                prefix: Vec::new(),
            }))
        })
        .collect();
    out.push(RenderRow::Table(Box::new(TableRow {
        header,
        cells: Vec::new(),
        kind: TableKind::Footer,
        prefix: Vec::new(),
    })));
    out
}

impl TableRow {
    fn project(&self, width: usize) -> Vec<Line<'static>> {
        self.project_with_prefix(width, false)
    }

    fn project_with_prefix(&self, width: usize, force_collapse: bool) -> Vec<Line<'static>> {
        let prefix_width: usize = self.prefix.iter().map(Span::width).sum();
        let collapsed = force_collapse || prefix_width > width.saturating_sub(2);
        let prefix = if !collapsed {
            self.prefix.clone()
        } else if width >= 3 {
            vec![Span::styled("…", Style::new().dim())]
        } else {
            Vec::new()
        };
        let used: usize = prefix.iter().map(Span::width).sum();
        let available_width = width.saturating_sub(used).clamp(1, MAX_TABLE_COLUMNS);
        let count = self.header.alignments.len();
        let mut out = Vec::new();
        if collapsed && prefix.is_empty() {
            out.push(table_notice("…", width));
        }
        let mut clipped = false;
        if available_width < count.saturating_mul(9).saturating_add(1) {
            let columns = if self.kind == TableKind::Header {
                self.cells.len()
            } else if self.kind == TableKind::Body {
                count.max(self.cells.len())
            } else {
                0
            };
            for column in 0..columns {
                let (lines, lost) = if self.kind == TableKind::Header {
                    table_cell(
                        &self.cells[column],
                        true,
                        available_width,
                        MAX_TABLE_RENDERED_ROWS.saturating_sub(out.len()),
                    )
                } else {
                    let mut spans = self.header.cells.get(column).cloned().unwrap_or_default();
                    if spans.is_empty() {
                        spans.push(Span::raw(format!("Column {}", column + 1)));
                    }
                    for span in &mut spans {
                        span.style = span.style.bold();
                    }
                    spans.push(Span::raw(": "));
                    if let Some(cell) = self.cells.get(column) {
                        spans.extend(cell.clone());
                    }
                    {
                        let (rows, clipped) = text::wrap(
                            &spans,
                            available_width,
                            MAX_TABLE_RENDERED_ROWS.saturating_sub(out.len()),
                            Table,
                        );
                        (rows.into_iter().map(|(line, _)| line).collect(), clipped)
                    }
                };
                out.extend(lines);
                if lost {
                    clipped = true;
                    break;
                }
            }
        } else {
            let available = available_width.saturating_sub(count * 3 + 1).max(count);
            let mut widths: Vec<_> = (0..count)
                .map(|i| {
                    self.header.cells.get(i).map_or(12, |spans| {
                        spans.iter().map(Span::width).sum::<usize>().max(12)
                    })
                })
                .collect();
            if widths.iter().sum::<usize>() > available {
                widths = (0..count)
                    .map(|column| available / count + usize::from(column < available % count))
                    .collect();
            } else {
                let extra = available - widths.iter().sum::<usize>();
                for (column, width) in widths.iter_mut().enumerate() {
                    *width += extra / count + usize::from(column < extra % count);
                }
            }
            let rule = |left: char, mid: char, right: char| {
                let body = widths
                    .iter()
                    .map(|width| "─".repeat(width + 2))
                    .collect::<Vec<_>>()
                    .join(&mid.to_string());
                Line::from(format!("{left}{body}{right}")).style(Style::new().dim())
            };
            match self.kind {
                TableKind::Header => out.push(rule('┌', '┬', '┐')),
                TableKind::Body => out.push(rule('├', '┼', '┤')),
                TableKind::Footer => out.push(rule('└', '┴', '┘')),
            }
            if self.kind != TableKind::Footer {
                let cells: Vec<_> = widths
                    .iter()
                    .enumerate()
                    .map(|(i, width)| {
                        table_cell(
                            self.cells.get(i).map_or(&[], Vec::as_slice),
                            self.kind == TableKind::Header,
                            *width,
                            MAX_TABLE_RENDERED_ROWS,
                        )
                    })
                    .collect();
                let height = cells.iter().map(|(rows, _)| rows.len()).max().unwrap_or(0);
                let room = MAX_TABLE_RENDERED_ROWS.saturating_sub(out.len());
                clipped = height > room || cells.iter().any(|(_, lost)| *lost);
                for y in 0..height.min(room) {
                    let mut spans = vec![Span::styled("│", Style::new().dim())];
                    for (column, (cell, _)) in cells.iter().enumerate() {
                        let line = cell.get(y).cloned().unwrap_or_default();
                        let gap = widths[column].saturating_sub(line.width());
                        let left = match self.header.alignments[column] {
                            Alignment::Right => gap,
                            Alignment::Center => gap / 2,
                            _ => 0,
                        };
                        spans.push(Span::raw(" ".repeat(left + 1)));
                        spans.extend(line.spans);
                        spans.push(Span::raw(" ".repeat(gap - left + 1)));
                        spans.push(Span::styled("│", Style::new().dim()));
                    }
                    out.push(Line::from(spans));
                }
            }
        }
        let prefix_bytes: usize = prefix
            .iter()
            .map(|span| size_of::<Span<'static>>() + span.content.len())
            .sum();
        if !collapsed && out.len().saturating_mul(prefix_bytes) > 2 * 1024 * 1024 {
            return self.project_with_prefix(width, true);
        }
        for line in &mut out {
            line.spans.splice(0..0, prefix.clone());
        }
        if clipped {
            if out.len() == MAX_TABLE_RENDERED_ROWS {
                out.pop();
            }
            out.push(table_notice(
                "table clipped: display row limit exceeded",
                width,
            ));
        }
        out
    }
}

const STREAM_WIDTH: u16 = u16::MAX;

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct ByteOffset(usize);

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct LineCount(usize);

struct Fence {
    marker: char,
    language: String,
    scanned_to: ByteOffset,
    rendered_to: ByteOffset,
    stable_lines: LineCount,
    lines: Vec<RenderRow>,
}

struct CommittablePartition {
    commit_end: ByteOffset,
    tail_events: Vec<Event<'static>>,
}

impl Fence {
    fn establish(text: &str) -> Option<Self> {
        let (opening, _) = text.split_once('\n')?;
        let marker = opening.chars().next()?;
        if !matches!(marker, '`' | '~') {
            return None;
        }
        let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(language))) =
            Parser::new_ext(text, OPTIONS).next()?
        else {
            return None;
        };
        Some(Self {
            marker,
            language: language.to_string(),
            scanned_to: ByteOffset(opening.len() + 1),
            rendered_to: ByteOffset(opening.len() + 1),
            stable_lines: LineCount(1),
            lines: vec![RenderRow::Literal(Line::default())],
        })
    }

    fn extend(&mut self, text: &str) -> bool {
        let delta = &text[self.scanned_to.0..];
        if delta.contains(self.marker) || delta.contains('\r') {
            return false;
        }
        self.scanned_to = ByteOffset(text.len());
        true
    }

    fn live(&mut self, text: &str) {
        self.lines.truncate(self.stable_lines.0);
        let kind = CodeBlockKind::Fenced(self.language.as_str().into());
        let width = usize::from(STREAM_WIDTH);
        let rendered = self.rendered_to.0;
        if let Some(end) = text.rfind('\n').filter(|&at| at >= rendered) {
            self.lines
                .extend(literals(code_lines(&text[rendered..end], &kind, width)));
            self.rendered_to = ByteOffset(end + 1);
            self.stable_lines = LineCount(self.lines.len());
        }
        if self.rendered_to.0 < text.len() {
            self.lines.extend(literals(code_lines(
                &text[self.rendered_to.0..],
                &kind,
                width,
            )));
        }
        if self.lines.len() == 1 {
            self.lines.push(RenderRow::Literal(Line::default()));
        }
    }
}

#[derive(Default)]
pub(super) struct Stream {
    filter: crate::terminal_text::Display,
    block: Clean,
    references: References,
    revision: u64,
    checked: u64,
    layout_bytes: usize,
    pending_since: Option<Instant>,
    emitted_lines: LineCount,
    fence: Option<Fence>,
    live: Option<(u64, Vec<RenderRow>)>,
}

impl Stream {
    fn preview_expired(&self) -> bool {
        self.pending_since
            .is_some_and(|since| since.elapsed() >= Duration::from_millis(150))
    }
    fn needs_layout(&self) -> bool {
        const EAGER_BYTES: usize = 16 * 1024;
        let laid_out = self.layout_bytes;
        self.block.len() <= EAGER_BYTES
            || laid_out == 0
            || self.block.len() >= laid_out + laid_out / 4
            || self.preview_expired()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.block.trim().is_empty()
    }
    pub(super) fn append(&mut self, delta: &str) {
        let before = self.block.len();
        self.block.append(delta, &mut self.filter);
        if self.block.len() != before {
            self.revision += 1;
            self.pending_since.get_or_insert_with(Instant::now);
        }
    }
    pub(super) fn flush(&mut self) -> Vec<RenderRow> {
        if self.checked == self.revision && !self.preview_expired() {
            return Vec::new();
        }
        self.checked = self.revision;
        let width = STREAM_WIDTH;
        let needs_layout = self.needs_layout();
        let fence = &mut self.fence;
        if fence.is_none() {
            if !needs_layout {
                return Vec::new();
            }
            *fence = Fence::establish(&self.block);
        }
        if let Some(open) = fence {
            if open.extend(&self.block) {
                open.live(&self.block);
                let end = open.stable_lines.max(self.emitted_lines);
                let lines = open.lines[self.emitted_lines.0..end.0].to_vec();
                self.emitted_lines = end;
                self.pending_since = None;
                return lines;
            }
            *fence = None;
        }
        self.pending_since = None;
        self.layout_bytes = self.block.len();
        let CommittablePartition {
            commit_end,
            tail_events,
        } = partition_committable(&self.block, &mut self.references);
        let tail = self.block[commit_end.0..].trim_matches(['\n', '\r']);
        let lines = render_events(tail, width, tail_events.into_iter());
        let lines = separate(lines);
        self.live = Some((self.revision, lines));
        if commit_end == ByteOffset(0) {
            let complete = self.block.rfind('\n').map_or("", |at| &self.block[..at]);
            let candidate = complete.rfind('\n').map_or("", |at| &complete[..=at]);
            if Fence::establish(candidate).is_none() && unstable_inline(candidate, &self.references)
            {
                return Vec::new();
            }
            let stable = separated_with(candidate, width, &self.references);
            let live = &self.live.as_ref().expect("set above").1;
            let end = LineCount(stable.iter().zip(live).take_while(|(a, b)| a == b).count());
            let end = end.max(self.emitted_lines);
            let lines = live[self.emitted_lines.0..end.0].to_vec();
            self.emitted_lines = end;
            return lines;
        }
        let finished = self.block[..commit_end.0].to_owned();
        self.block = Clean::new(&self.block[commit_end.0..]);
        self.layout_bytes = self.block.len();
        separated_with(&finished, width, &self.references)
            .into_iter()
            .skip(std::mem::take(&mut self.emitted_lines).0)
            .collect()
    }
    pub(super) fn finish(&mut self) -> Vec<RenderRow> {
        let lines = separated_with(&self.block, STREAM_WIDTH, &self.references)
            .into_iter()
            .skip(self.emitted_lines.0)
            .collect();
        *self = Self::default();
        lines
    }
    pub(super) fn live(&mut self) -> &[RenderRow] {
        let needs_layout = self.needs_layout();
        if let Some(fence) = &mut self.fence {
            fence.live(&self.block);
            let end = fence
                .lines
                .iter()
                .rposition(|line| !line.is_blank())
                .map_or(2, |at| at + 1);
            return &fence.lines[self.emitted_lines.0.min(end)..end];
        }
        let revision = self.live.as_ref().map(|(revision, _)| *revision);
        if revision.is_none() || revision != Some(self.revision) && needs_layout {
            self.layout_bytes = self.block.len();
            self.live = Some((
                self.revision,
                separated_with(&self.block, STREAM_WIDTH, &self.references),
            ));
        }
        let lines = &self.live.as_ref().expect("prepared above").1;
        &lines[self.emitted_lines.0.min(lines.len())..]
    }
}

fn unstable_inline(text: &str, references: &References) -> bool {
    enum Context {
        Prose,
        Table,
        Code,
    }
    if text.contains("[^") || latex_delimiters(text).1.is_some() {
        return true;
    }
    let mut context = Context::Prose;
    parser(text, text, references).any(|event| match event {
        Event::Start(Tag::Table(_)) => {
            context = Context::Table;
            false
        }
        Event::Start(Tag::CodeBlock(_)) => {
            context = Context::Code;
            false
        }
        Event::End(pulldown_cmark::TagEnd::Table | pulldown_cmark::TagEnd::CodeBlock) => {
            context = Context::Prose;
            false
        }
        Event::Text(_) if matches!(context, Context::Code) => false,
        Event::Text(text) if matches!(context, Context::Table) => text.contains('['),
        Event::Text(text) => {
            text.contains(['*', '`', '[', '~', '$'])
                || text.match_indices('_').any(|(at, _)| {
                    !text[..at]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric)
                        || !text[at + 1..]
                            .chars()
                            .next()
                            .is_some_and(char::is_alphanumeric)
                })
        }
        Event::Start(Tag::Link { link_type, id, .. } | Tag::Image { link_type, id, .. }) => {
            !matches!(
                link_type,
                LinkType::Inline | LinkType::Autolink | LinkType::Email
            ) && lookup(references, &id).is_none()
        }
        _ => false,
    })
}

fn separated_with(text: &str, width: u16, references: &References) -> Vec<RenderRow> {
    separate(render_filtered(text, width, references))
}

fn separate(lines: Vec<RenderRow>) -> Vec<RenderRow> {
    if lines.is_empty() {
        return lines;
    }
    std::iter::once(RenderRow::Literal(Line::default()))
        .chain(lines)
        .collect()
}

fn lookup<'a>(references: &'a References, label: &str) -> Option<&'a Reference> {
    references.get(&unicase::UniCase::new(label.to_owned()))
}

fn parser<'a>(
    text: &'a str,
    original: &'a str,
    references: &'a References,
) -> impl Iterator<Item = Event<'a>> {
    let callback = |link: BrokenLink<'a>| {
        lookup(references, &link.reference)
            .map(|def| (def.url.clone().into(), def.title.clone().into()))
    };
    Parser::new_with_broken_link_callback(text, OPTIONS, Some(callback))
        .into_offset_iter()
        .map(|(event, range)| adjusted_event(event, range.start, original, references))
}

fn adjusted_event<'a>(
    mut event: Event<'a>,
    start: usize,
    original: &str,
    references: &References,
) -> Event<'a> {
    if let Event::InlineMath(math) = &event
        && original[start..].starts_with(r"\(")
        && let Some(math) = math
            .strip_prefix('{')
            .and_then(|math| math.strip_suffix('}'))
    {
        event = Event::InlineMath(math.trim().to_owned().into());
    }
    if let Event::Start(
        Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }
        | Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        },
    ) = &mut event
        && !matches!(
            link_type,
            LinkType::Inline | LinkType::Autolink | LinkType::Email
        )
        && let Some(def) = lookup(references, id)
    {
        *dest_url = def.url.clone().into();
        *title = def.title.clone().into();
    }
    event
}

fn latex_delimiters(text: &str) -> (Cow<'_, str>, Option<usize>) {
    if !text.contains(r"\(") && !text.contains(r"\[") {
        return (Cow::Borrowed(text), None);
    }
    let mut protected = Vec::new();
    let mut code_blocks = Vec::new();
    let mut inline_blocks = Vec::new();
    let mut code_start = None;
    for (event, range) in Parser::new_ext(text, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_start = Some(range.start),
            Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
                if let Some(start) = code_start.take() {
                    protected.push(start..range.end);
                    code_blocks.push(start..range.end);
                }
            }
            Event::Code(_) | Event::InlineMath(_) | Event::DisplayMath(_) => protected.push(range),
            Event::Start(Tag::Paragraph | Tag::Heading { .. } | Tag::Item | Tag::TableCell)
            | Event::Rule => inline_blocks.push(range.start),
            _ => {}
        }
    }
    inline_blocks.sort_unstable();
    let escaped = |at| {
        text.as_bytes()[..at]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count()
            % 2
            != 0
    };
    let mut normalized = Cow::Borrowed(text);
    let mut at = 0;
    let mut unfinished = None;
    while let Some(offset) = text[at..].find('\\') {
        at += offset;
        let next = protected.get(protected.partition_point(|range| range.end <= at));
        if let Some(range) = next
            && range.start <= at
        {
            at = range.end;
            continue;
        }
        let close = match text.as_bytes().get(at + 1) {
            Some(b'(') if !escaped(at) => r"\)",
            Some(b'[') if !escaped(at) => r"\]",
            _ => {
                at += 1;
                continue;
            }
        };
        let next_block = code_blocks.get(code_blocks.partition_point(|range| range.end <= at));
        let mut limit = next_block.map_or(text.len(), |range| range.start);
        if close == r"\)"
            && let Some(&start) =
                inline_blocks.get(inline_blocks.partition_point(|&start| start <= at))
        {
            limit = limit.min(start);
        }
        let mut end = at + 2;
        while end < limit {
            let byte = text.as_bytes()[end];
            if byte == b'\\' && text[end..limit].starts_with(close) && !escaped(end) {
                break;
            }
            if byte == b'\n' && close == r"\)" {
                let mut after = end + 1;
                while after < limit && matches!(text.as_bytes()[after], b' ' | b'\t' | b'\r') {
                    after += 1;
                }
                if text.as_bytes().get(after) == Some(&b'\n') && after < limit {
                    limit = end;
                    break;
                }
            }
            end += 1;
        }
        if end >= limit {
            if limit == text.len() {
                unfinished.get_or_insert(at);
            }
            at = limit;
            continue;
        }
        let inline = close == r"\)";
        // Equal byte lengths preserve the streaming parser's source offsets.
        normalized
            .to_mut()
            .replace_range(end..end + 2, if inline { "}$" } else { "$$" });
        normalized
            .to_mut()
            .replace_range(at..at + 2, if inline { "${" } else { "$$" });
        at = end + 2;
    }
    (normalized, unfinished)
}

fn partition_committable(text: &str, references: &mut References) -> CommittablePartition {
    let whole = text.rfind('\n').map_or("", |last| &text[..=last]);
    let source = text;
    let (normalized, unfinished) = latex_delimiters(source);
    let unresolved = Cell::new(unfinished.unwrap_or(usize::MAX));
    let callback = |link: BrokenLink<'_>| {
        if let Some(def) = lookup(references, &link.reference) {
            Some((def.url.clone().into(), def.title.clone().into()))
        } else {
            unresolved.set(unresolved.get().min(link.span.start));
            None
        }
    };
    let parser = Parser::new_with_broken_link_callback(&normalized, OPTIONS, Some(callback));
    let definitions: HashMap<_, _> = parser
        .reference_definitions()
        .iter()
        .map(|(label, def)| {
            (
                unicase::UniCase::new(label.to_owned()),
                (
                    Reference {
                        url: def.dest.to_string(),
                        title: def
                            .title
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                    },
                    def.span.end,
                ),
            )
        })
        .collect();
    let mut depth = 0;
    let mut starts = Vec::new();
    let mut dependencies = Vec::new();
    let events: Vec<_> = parser
        .into_offset_iter()
        .map(|(event, range)| (event.into_static(), range))
        .collect();
    for (event, range) in &events {
        match event {
            Event::Start(tag) => {
                if depth == 0 && range.start < whole.len() {
                    starts.push(text[..range.start].rfind('\n').map_or(0, |at| at + 1));
                }
                if let Tag::Link { link_type, id, .. } | Tag::Image { link_type, id, .. } = tag
                    && !matches!(
                        link_type,
                        LinkType::Inline | LinkType::Autolink | LinkType::Email
                    )
                    && lookup(references, id).is_none()
                    && let Some((_, definition_end)) =
                        definitions.get(&unicase::UniCase::new(id.to_string()))
                {
                    dependencies.push((*starts.last().unwrap_or(&0), *definition_end));
                }
                depth += 1;
            }
            Event::End(_) => depth -= 1,
            Event::Rule if depth == 0 && range.start < whole.len() => {
                starts.push(text[..range.start].rfind('\n').map_or(0, |at| at + 1));
            }
            _ => {}
        }
    }
    let mut end = starts.last().copied().unwrap_or(0);
    if let Some(start) = starts
        .iter()
        .rev()
        .find(|&&start| start <= unresolved.get())
        && unresolved.get() != usize::MAX
    {
        end = end.min(*start);
    }
    for &(start, definition_end) in dependencies.iter().rev() {
        if definition_end > end {
            end = end.min(start);
        }
    }
    if text.contains("[^") {
        end = 0;
    }
    for (label, (def, definition_end)) in definitions {
        if definition_end <= end {
            references.entry(label).or_insert(def);
        }
    }
    let tail_events = events
        .into_iter()
        .filter(|(_, range)| range.start >= end)
        .map(|(event, range)| adjusted_event(event, range.start, source, references))
        .collect();
    CommittablePartition {
        commit_end: ByteOffset(end),
        tail_events,
    }
}

#[cfg(test)]
mod tests;
