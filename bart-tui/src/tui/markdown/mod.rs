use crate::tui::text::Clean;
use crate::tui::text::Wrap::{self, Plain, Table, Verbatim};
mod math;

use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use super::{
    selection::{self, CellBox, Point, Row},
    text,
};
use pulldown_cmark::{Alignment, BrokenLink, CodeBlockKind, Event, LinkType, Options, Parser, Tag};
use ratatui::{
    style::{Style, Stylize},
    text::{Line, Span, Text},
};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RenderRow {
    kind: RowKind,
    prefix: Vec<Span<'static>>,
    wrap_prefix: Vec<Span<'static>>,
}

#[derive(Clone, Debug, PartialEq)]
enum RowKind {
    Text(Line<'static>, Wrap),
    Table(Box<TableRow>),
    TableClipped,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TableRow {
    header: Arc<TableHeader>,
    cells: Vec<Vec<Span<'static>>>,
    kind: TableKind,
}

#[derive(Clone, Debug, PartialEq)]
struct TableHeader {
    cells: Vec<Vec<Span<'static>>>,
    alignments: Vec<Alignment>,
    geometry: Arc<TableGeometry>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TableKind {
    Header,
    FirstBody,
    Body,
}

#[derive(Debug, PartialEq)]
struct TableGeometry {
    preferred: Box<[usize]>,
}

impl RenderRow {
    fn new(kind: RowKind) -> Self {
        Self {
            kind,
            prefix: Vec::new(),
            wrap_prefix: Vec::new(),
        }
    }
    fn text(line: Line<'static>, wrap: Wrap) -> Self {
        Self::new(RowKind::Text(line, wrap))
    }
    pub(super) fn literal(line: Line<'static>) -> Self {
        Self::text(line, Wrap::Prose)
    }
    pub(super) fn source(&self) -> String {
        match &self.kind {
            RowKind::Text(line, _) => span_text(&line.spans),
            RowKind::Table(_) if self.table_clipped() => TABLE_BYTES_NOTICE.into(),
            RowKind::Table(row) => row.source(0).markdown(true),
            RowKind::TableClipped => TABLE_BYTES_NOTICE.into(),
        }
    }
    pub(super) fn logical_line(&self) -> Cow<'_, Line<'static>> {
        match &self.kind {
            RowKind::Text(line, _) => Cow::Borrowed(line),
            RowKind::Table(_) | RowKind::TableClipped => {
                Cow::Owned(Line::from(Span::raw(self.source())))
            }
        }
    }
    #[cfg(test)]
    pub(super) fn project(&self, width: u16) -> Vec<Line<'static>> {
        selectable(std::slice::from_ref(self), 0, width)
            .into_iter()
            .map(Row::into_line)
            .collect()
    }
    fn table_clipped(&self) -> bool {
        match &self.kind {
            RowKind::Table(_) => self.bytes() > MAX_TABLE_ROW_BYTES,
            RowKind::TableClipped => true,
            RowKind::Text(..) => false,
        }
    }
    pub(super) fn source_extent(&self) -> usize {
        let len = match &self.kind {
            RowKind::Text(line, _) => line.spans.iter().map(|span| span.content.len()).sum(),
            RowKind::Table(_) if self.table_clipped() => TABLE_BYTES_NOTICE.len(),
            RowKind::Table(row) => row.source_len(),
            RowKind::TableClipped => TABLE_BYTES_NOTICE.len(),
        };
        len + 1
    }
    fn selectable(&self, at: Point, width: usize, budget: &mut TableBudget) -> Vec<Row> {
        match &self.kind {
            RowKind::Text(line, mode) => {
                selection::wrap_prefixed(line, &self.prefix, &self.wrap_prefix, at, width, *mode)
            }
            RowKind::Table(_) if self.table_clipped() => {
                if budget.remaining() > 0 {
                    budget.used += 1;
                    vec![Row::notice(
                        (at.0, at.1, 0),
                        table_notice(TABLE_BYTES_NOTICE, width),
                        TABLE_BYTES_NOTICE,
                    )]
                } else {
                    budget.clip(at, 0, width).into_iter().collect()
                }
            }
            RowKind::Table(row) => row.project(at, width, &self.prefix, budget),
            RowKind::TableClipped => {
                if budget.remaining() > 0 {
                    budget.used += 1;
                    vec![Row::notice(
                        (at.0, at.1, 0),
                        table_notice(TABLE_BYTES_NOTICE, width),
                        TABLE_BYTES_NOTICE,
                    )]
                } else {
                    budget.clip(at, 0, width).into_iter().collect()
                }
            }
        }
    }
    pub(super) fn bytes(&self) -> usize {
        fn spans(spans: &Vec<Span<'static>>) -> usize {
            spans.capacity() * size_of::<Span<'static>>()
                + spans
                    .iter()
                    .map(|span| match &span.content {
                        Cow::Borrowed(text) => text.len(),
                        Cow::Owned(text) => text.capacity(),
                    })
                    .sum::<usize>()
        }
        fn cells(cells: &Vec<Vec<Span<'static>>>) -> usize {
            cells.capacity() * size_of::<Vec<Span<'static>>>()
                + cells.iter().map(spans).sum::<usize>()
        }
        size_of::<Self>()
            + spans(&self.prefix)
            + spans(&self.wrap_prefix)
            + match &self.kind {
                RowKind::Text(line, _) => spans(&line.spans),
                RowKind::TableClipped => 0,
                RowKind::Table(row) => {
                    size_of::<TableRow>()
                        + size_of::<TableHeader>()
                        + cells(&row.cells)
                        + cells(&row.header.cells)
                        + row.header.geometry.bytes()
                        + row.header.alignments.capacity() * size_of::<Alignment>()
                }
            }
    }
    pub(super) fn bound_table(&mut self) {
        if self.table_clipped() {
            *self = Self::new(RowKind::TableClipped);
        }
    }
    pub(super) fn prefix(&mut self, prefix: Span<'static>) {
        self.prefixes(prefix.clone(), prefix);
    }
    pub(super) fn prefixes(&mut self, first: Span<'static>, rest: Span<'static>) {
        self.prefix.insert(0, first);
        self.wrap_prefix.insert(0, rest);
    }
    pub(super) fn is_blank(&self) -> bool {
        matches!(&self.kind, RowKind::Text(line, Wrap::Prose | Plain) if line.spans.iter().all(|span| span.content.trim().is_empty()))
    }
}

fn span_text(spans: &[Span<'static>]) -> String {
    spans.iter().map(|span| span.content.as_ref()).collect()
}

pub(super) fn selectable(rows: &[RenderRow], block: u64, width: u16) -> Vec<Row> {
    let mut byte = 0;
    let mut budget = TableBudget::default();
    rows.iter()
        .flat_map(|row| {
            let lines = row.selectable((block, byte), usize::from(width.max(1)), &mut budget);
            byte += row.source_extent();
            lines
        })
        .collect()
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
                    RenderRow::literal(line)
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

struct Reference {
    url: String,
    title: String,
}
type References = HashMap<unicase::UniCase<String>, Reference>;

const MAX_TABLE_COLUMNS: usize = 120;
const MAX_TABLE_ROW_BYTES: usize = 64 * 1024;
const MAX_TABLE_RENDERED_ROWS: usize = 32 * 1024;
const MAX_TABLE_PREFIX_BYTES: usize = 512;
const TABLE_BYTES_NOTICE: &str = "table clipped: display limit exceeded";
const TABLE_ROWS_NOTICE: &str = "table clipped: display row limit exceeded";

#[derive(Default)]
struct TableBudget {
    used: usize,
    clipped: bool,
}

impl TableBudget {
    fn remaining(&self) -> usize {
        if self.clipped {
            0
        } else {
            MAX_TABLE_RENDERED_ROWS - 1 - self.used
        }
    }
    fn clip(&mut self, (block, byte): Point, ordinal: usize, width: usize) -> Option<Row> {
        (!std::mem::replace(&mut self.clipped, true)).then(|| {
            Row::notice(
                (block, byte, ordinal),
                table_notice(TABLE_ROWS_NOTICE, width),
                TABLE_ROWS_NOTICE,
            )
        })
    }
}

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

pub(super) fn render_detail(markdown: &str, width: u16) -> Vec<Row> {
    selectable(&render_filtered(markdown, &References::new()), 0, width)
}

fn render_filtered(markdown: &str, references: &References) -> Vec<RenderRow> {
    let (normalized, _) = latex_delimiters(markdown);
    render_events(markdown, parser(&normalized, markdown, references))
}

fn render_events<'a>(
    markdown: &str,
    mut events: impl Iterator<Item = Event<'a>>,
) -> Vec<RenderRow> {
    let Some(nodes) = nodes(&mut events, 0) else {
        return vec![RenderRow::text(
            Line::from(Span::raw(markdown.to_owned())),
            Plain,
        )];
    };
    block_lines(&nodes, false)
}

fn inline(nodes: &[Node<'_>], style: Style, out: &mut Vec<Span<'static>>) {
    for node in nodes {
        match node {
            Node::Element(Tag::Link { dest_url, .. }, children) => {
                let link = crate::tui::links::markdown_style(style, dest_url);
                let label_style = link.unwrap_or_else(|| style.underlined());
                let start = out.len();
                inline(children, label_style, out);
                let label = &out[start..];
                if label.iter().all(|span| span.content.trim().is_empty()) {
                    out.truncate(start);
                    out.push(Span::styled(Clean::new(dest_url), label_style));
                } else if link.is_none()
                    && !label
                        .iter()
                        .flat_map(|span| span.content.bytes())
                        .eq(dest_url.bytes())
                {
                    out.push(Span::styled(Clean::new(&format!(" ({dest_url})")), style));
                }
            }
            Node::Element(tag, children) => {
                let next = match tag {
                    Tag::Emphasis => style.italic(),
                    Tag::Strong => style.bold(),
                    Tag::Strikethrough => style.crossed_out(),
                    _ => style,
                };
                inline(children, next, out);
            }
            Node::Leaf(event) => match event {
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    out.push(Span::styled(Clean::new(text), style));
                }
                Event::InlineMath(text) => out.push(Span::styled(
                    math::present(text),
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

fn marked(mut lines: Vec<RenderRow>, marker: String) -> Vec<RenderRow> {
    let indent = Span::raw(" ".repeat(text::width(&marker)));
    match lines.first_mut() {
        Some(RenderRow {
            kind: RowKind::Text(line, Plain),
            prefix,
            wrap_prefix,
        }) if prefix.is_empty() => {
            line.spans.insert(0, Span::raw(marker));
            wrap_prefix.insert(0, indent.clone());
            for row in &mut lines[1..] {
                row.prefix(indent.clone());
            }
        }
        _ => {
            for row in &mut lines {
                row.prefix(indent.clone());
            }
            lines.insert(0, RenderRow::text(Line::raw(marker), Plain));
        }
    }
    lines
}

fn push_prose(out: &mut Vec<RenderRow>, pending: &mut Vec<Span<'static>>) {
    if !pending.is_empty() {
        out.push(RenderRow::text(Line::from(std::mem::take(pending)), Plain));
    }
}

fn block_lines(nodes: &[Node<'_>], compact: bool) -> Vec<RenderRow> {
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
                push_prose(&mut out, &mut pending);
                if !out.is_empty() {
                    out.push(RenderRow::literal(Line::default()));
                }
                out.push(RenderRow::literal(Line::styled("───", Style::new().dim())));
            } else {
                inline(std::slice::from_ref(node), Style::new(), &mut pending);
            }
            continue;
        };
        let tight = !pending.is_empty() && matches!(tag, Tag::List(_));
        push_prose(&mut out, &mut pending);
        let lines = match tag {
            Tag::Paragraph | Tag::Heading { .. } | Tag::HtmlBlock => prose(tag, children),
            Tag::CodeBlock(kind) => code(children, kind),
            Tag::BlockQuote(_) => {
                let mut rows = block_lines(children, false);
                for row in &mut rows {
                    row.prefix(Span::styled("│ ", super::transcript::muted(Style::new())));
                }
                rows
            }
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
                        lines.extend(marked(block_lines(body, true), marker));
                        number += 1;
                    }
                }
                lines
            }
            Tag::FootnoteDefinition(label) => {
                let marker = Clean::new(&format!("[^{label}]: ")).to_string();
                marked(block_lines(children, false), marker)
            }
            Tag::Table(alignments) => table(children, alignments),
            _ => block_lines(children, false),
        };
        if !out.is_empty() && !tight && !compact && !matches!(tag, Tag::Item) {
            out.push(RenderRow::literal(Line::default()));
        }
        out.extend(lines);
    }
    push_prose(&mut out, &mut pending);
    out
}

fn prose(tag: &Tag<'_>, children: &[Node<'_>]) -> Vec<RenderRow> {
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
            push_prose(&mut lines, &mut spans);
            let style = Style::new().fg(super::theme().code);
            let mut row = RenderRow::text(
                Line::from(Span::styled(math::present(text.trim_matches('\n')), style)),
                Plain,
            );
            row.prefix(Span::styled("  ", style));
            lines.push(row);
        } else {
            inline(std::slice::from_ref(child), style, &mut spans);
        }
    }
    if matches!(tag, Tag::HtmlBlock)
        && let Some(last) = spans.last_mut()
    {
        let trimmed = last.content.strip_suffix('\n');
        if let Some(trimmed) = trimmed {
            last.content = trimmed.to_owned().into();
        }
    }
    push_prose(&mut lines, &mut spans);
    lines
}

fn code(children: &[Node<'_>], kind: &CodeBlockKind<'_>) -> Vec<RenderRow> {
    let mut spans = Vec::new();
    inline(children, Style::new(), &mut spans);
    let code = span_text(&spans);
    let code = code.strip_suffix('\n').unwrap_or(&code);
    let mut style = CodeStyle::new(kind);
    code.split('\n').map(|line| style.row(line)).collect()
}

#[derive(Clone)]
struct CodeStyle(Option<super::highlight::Syntax>);

impl CodeStyle {
    fn new(kind: &CodeBlockKind<'_>) -> Self {
        let syntax = match kind {
            CodeBlockKind::Fenced(lang) => lang
                .split_whitespace()
                .next()
                .and_then(super::highlight::Syntax::new),
            CodeBlockKind::Indented => None,
        };
        Self(syntax)
    }
    fn row(&mut self, text: &str) -> RenderRow {
        if let Some(line) = self.0.as_mut().and_then(|syntax| syntax.line(text)) {
            return RenderRow::text(line, Verbatim);
        }
        self.0 = None;
        RenderRow::text(
            Line::styled(text.to_owned(), Style::new().fg(super::theme().code)),
            Verbatim,
        )
    }
}

fn table_cell(
    spans: &[Span<'static>],
    header: bool,
    widths: [usize; 2],
    limit: usize,
) -> (Vec<text::Wrapped>, bool) {
    if header {
        let spans: Vec<_> = spans.iter().cloned().map(Stylize::bold).collect();
        text::wrap(&spans, widths, limit, Table)
    } else {
        text::wrap(spans, widths, limit, Table)
    }
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
    let geometry = Arc::new(TableGeometry::new(&rows, alignments.len()));
    let header = Arc::new(TableHeader {
        cells: rows[0].clone(),
        alignments: alignments.to_vec(),
        geometry: Arc::clone(&geometry),
    });
    rows.drain(..)
        .enumerate()
        .map(|(index, cells)| {
            RenderRow::new(RowKind::Table(Box::new(TableRow {
                header: Arc::clone(&header),
                cells,
                kind: match index {
                    0 => TableKind::Header,
                    1 => TableKind::FirstBody,
                    _ => TableKind::Body,
                },
            })))
        })
        .collect()
}

const TABLE_GUTTER: usize = 2;
const MIN_TABLE_COLUMN: usize = 4;

impl TableGeometry {
    fn new(rows: &[Vec<Vec<Span<'static>>>], count: usize) -> Self {
        let preferred = if count > 0 && count <= MAX_TABLE_COLUMNS.div_ceil(1 + TABLE_GUTTER) {
            (0..count)
                .map(|column| {
                    rows.iter()
                        .filter_map(|row| row.get(column))
                        .map(|spans| spans.iter().map(Span::width).sum::<usize>())
                        .max()
                        .unwrap_or(1)
                        .clamp(1, MAX_TABLE_COLUMNS + 1)
                })
                .collect()
        } else {
            Box::default()
        };
        Self { preferred }
    }

    fn bytes(&self) -> usize {
        size_of::<Self>() + size_of_val(self.preferred.as_ref())
    }

    fn layout<'a>(prefix: &'a [Span<'static>], width: usize) -> (&'a [Span<'static>], usize) {
        const COLLAPSED: &[Span<'static>] = &[Span {
            content: Cow::Borrowed("…"),
            style: Style::new().dim(),
        }];
        let room = width.min(2);
        let prefix = selection::fit_prefix(prefix, width, room);
        let bytes: usize = prefix
            .iter()
            .map(|span| size_of::<Span<'static>>() + span.content.len())
            .sum();
        let prefix = if bytes > MAX_TABLE_PREFIX_BYTES {
            selection::fit_prefix(COLLAPSED, width, room)
        } else {
            prefix
        };
        let available =
            (width - prefix.iter().map(Span::width).sum::<usize>()).min(MAX_TABLE_COLUMNS);
        (prefix, available)
    }

    fn columns(&self, width: usize) -> Option<Vec<usize>> {
        let count = self.preferred.len();
        if count == 0 {
            return None;
        }
        let gutters = count.saturating_sub(1) * TABLE_GUTTER;
        let mins: Vec<usize> = self
            .preferred
            .iter()
            .map(|&pref| pref.clamp(1, MIN_TABLE_COLUMN))
            .collect();
        let min_total: usize = mins.iter().sum();
        if width < min_total + gutters {
            return None;
        }
        let available = width - gutters;
        let total: usize = self.preferred.iter().sum();
        if total <= available {
            return Some(self.preferred.to_vec());
        }
        let mut widths = mins;
        let mut remaining = available - min_total;
        while remaining > 0 {
            let unfinished: Vec<usize> = (0..count)
                .filter(|&col| widths[col] < self.preferred[col])
                .collect();
            if unfinished.is_empty() {
                break;
            }
            let share = (remaining / unfinished.len()).max(1);
            for col in unfinished {
                if remaining == 0 {
                    break;
                }
                let need = self.preferred[col] - widths[col];
                let grant = need.min(share).min(remaining);
                widths[col] += grant;
                remaining -= grant;
            }
        }
        Some(widths)
    }
}

fn table_rule(widths: &[usize], glyph: char) -> Span<'static> {
    let total = widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * TABLE_GUTTER;
    Span::styled(glyph.to_string().repeat(total), Style::new().dim())
}

impl TableRow {
    fn source(&self, byte: usize) -> selection::Table {
        let alignments = &self.header.alignments;
        let separator = (self.kind == TableKind::Header).then(|| {
            let rules = alignments
                .iter()
                .map(|alignment| match alignment {
                    Alignment::None => "---",
                    Alignment::Left => ":---",
                    Alignment::Center => ":---:",
                    Alignment::Right => "---:",
                })
                .collect::<Vec<_>>();
            format!("| {} |", rules.join(" | "))
        });
        selection::Table::new(
            byte,
            (0..alignments.len()).map(|index| span_text(self.cell(index))),
            separator,
        )
    }

    fn cell(&self, index: usize) -> &[Span<'static>] {
        self.cells.get(index).map_or(&[], Vec::as_slice)
    }

    fn source_len(&self) -> usize {
        let count = self.header.alignments.len();
        count
            + 1
            + self
                .cells
                .iter()
                .take(count)
                .flatten()
                .map(|span| span.content.len())
                .sum::<usize>()
    }

    fn project(
        &self,
        at: Point,
        width: usize,
        prefix: &[Span<'static>],
        budget: &mut TableBudget,
    ) -> Vec<Row> {
        let limit = budget.remaining();
        if limit == 0 {
            return budget.clip(at, 0, width).into_iter().collect();
        }
        let geometry = &self.header.geometry;
        let (prefix, available) = TableGeometry::layout(prefix, width);
        let mut projection = TableProjection {
            rows: Vec::new(),
            prefix,
            source: Arc::new(self.source(at.1)),
            limit,
        };
        let lost = match geometry.columns(available) {
            Some(widths) => self.boxed(&mut projection, &widths),
            None => self.stacked(&mut projection, available),
        };
        let mut rows = projection.finish(at);
        budget.used += rows.len();
        if lost
            && let Some(notice) = budget.clip((at.0, at.1 + self.source_len()), rows.len(), width)
        {
            rows.push(notice);
        }
        rows
    }

    fn stacked(&self, projection: &mut TableProjection<'_>, width: usize) -> bool {
        for column in 0..self.header.alignments.len() {
            if projection.rows.len() == projection.limit {
                return true;
            }
            let (start, _) = projection.source.cell(column);
            let mut label = if self.kind == TableKind::Header {
                Vec::new()
            } else {
                self.label(column)
            };
            let mut label_width: usize = label.iter().map(Span::width).sum();
            if !label.is_empty()
                && label_width + text::glyph_room(self.cell(column), width).max(2) > width
            {
                let clipped = text::clip_end(span_text(&label).trim_end(), width);
                let end = 1;
                projection.push(
                    Line::from(Span::raw(clipped).bold()),
                    vec![CellBox::new(width, column, start, end..end)],
                );
                label.clear();
                label_width = 0;
            }
            let (lines, lost) = table_cell(
                self.cell(column),
                self.kind == TableKind::Header,
                [width - label_width, width],
                projection.limit.saturating_sub(projection.rows.len()),
            );
            for (i, wrapped) in lines.into_iter().enumerate() {
                let mut spans = if i == 0 {
                    std::mem::take(&mut label)
                } else {
                    Vec::new()
                };
                let first = spans.len();
                spans.extend(wrapped.line.spans);
                let end = spans.len();
                projection.push(
                    Line::from(spans),
                    vec![CellBox::new(
                        width,
                        column,
                        start + wrapped.byte,
                        first..end,
                    )],
                );
            }
            if lost {
                return true;
            }
        }
        false
    }

    fn label(&self, column: usize) -> Vec<Span<'static>> {
        let mut label = self.header.cells.get(column).cloned().unwrap_or_default();
        if label.iter().all(|span| span.content.trim().is_empty()) {
            label = vec![Span::raw(format!("Column {}", column + 1))];
        } else {
            for span in &mut label {
                span.content = span.content.replace('\n', " ").into();
                span.style = Style::new();
            }
        }
        label.push(Span::raw(": "));
        label.into_iter().map(Stylize::bold).collect()
    }

    fn boxed(&self, projection: &mut TableProjection<'_>, widths: &[usize]) -> bool {
        let wraps = self
            .header
            .geometry
            .preferred
            .iter()
            .zip(widths)
            .any(|(&pref, &width)| pref > width);
        if self.kind == TableKind::Body && wraps && projection.limit > 1 {
            projection.push_rule(Line::from(table_rule(widths, '╌')), false);
        }
        let reserve_rule = usize::from(self.kind == TableKind::Header && projection.limit > 1);
        let room = projection.limit - projection.rows.len() - reserve_rule;
        let cells: Vec<_> = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                let (rows, lost) = table_cell(
                    self.cell(column),
                    self.kind == TableKind::Header,
                    [*width; 2],
                    room,
                );
                (rows.into_iter(), lost)
            })
            .collect();
        let height = cells.iter().map(|(rows, _)| rows.len()).max().unwrap_or(0);
        let lost = cells.iter().any(|(_, lost)| *lost);
        let mut cells: Vec<_> = cells.into_iter().map(|(rows, _)| rows).collect();
        let count = widths.len();
        for _ in 0..height {
            let mut spans = Vec::new();
            let mut col_start = 0;
            let mut boxes = Vec::with_capacity(count);
            for (column, (lines, width)) in cells.iter_mut().zip(widths).enumerate() {
                if column > 0 {
                    spans.push(Span::raw(" ".repeat(TABLE_GUTTER)));
                }
                let (start, end) = projection.source.cell(column);
                let (line, byte) = lines.next().map_or((Line::default(), end), |wrapped| {
                    (wrapped.line, start + wrapped.byte)
                });
                let gap = width.saturating_sub(line.width());
                let left = match self.header.alignments[column] {
                    Alignment::Right => gap,
                    Alignment::Center => gap / 2,
                    _ => 0,
                };
                if left > 0 {
                    spans.push(Span::raw(" ".repeat(left)));
                }
                let first = spans.len();
                spans.extend(line.spans);
                let end = spans.len();
                if column + 1 < count && gap > left {
                    spans.push(Span::raw(" ".repeat(gap - left)));
                }
                let right = col_start + width + usize::from(column + 1 < count);
                col_start += width + TABLE_GUTTER;
                boxes.push(CellBox::new(right, column, byte, first..end));
            }
            projection.push(Line::from(spans), boxes);
        }
        if self.kind == TableKind::Header && projection.rows.len() < projection.limit {
            projection.push_rule(Line::from(table_rule(widths, '─')), true);
        }
        lost
    }
}

enum ProjectedRow {
    Cells(Line<'static>, Vec<CellBox>),
    Rule { line: Line<'static>, at_end: bool },
}

struct TableProjection<'a> {
    rows: Vec<ProjectedRow>,
    prefix: &'a [Span<'static>],
    source: Arc<selection::Table>,
    limit: usize,
}

impl TableProjection<'_> {
    fn push(&mut self, line: Line<'static>, boxes: Vec<CellBox>) {
        self.rows.push(ProjectedRow::Cells(line, boxes));
    }

    fn push_rule(&mut self, line: Line<'static>, at_end: bool) {
        self.rows.push(ProjectedRow::Rule { line, at_end });
    }

    fn finish(self, (block, byte): Point) -> Vec<Row> {
        let end = self.source.end() + 1;
        self.rows
            .into_iter()
            .enumerate()
            .map(|(ordinal, row)| match row {
                ProjectedRow::Cells(line, boxes) => Row::table(
                    (block, byte, ordinal),
                    line,
                    self.prefix,
                    &self.source,
                    boxes,
                ),
                ProjectedRow::Rule { mut line, at_end } => {
                    line.spans.splice(..0, self.prefix.iter().cloned());
                    let point = (block, if at_end { end } else { byte });
                    Row::chrome_at((block, byte, ordinal), point, line)
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct ByteOffset(usize);

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct LineCount(usize);

struct Fence {
    marker: char,
    style: CodeStyle,
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
            style: CodeStyle::new(&CodeBlockKind::Fenced(language)),
            scanned_to: ByteOffset(opening.len() + 1),
            rendered_to: ByteOffset(opening.len() + 1),
            stable_lines: LineCount(1),
            lines: vec![RenderRow::literal(Line::default())],
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
        let rendered = self.rendered_to.0;
        if let Some(end) = text.rfind('\n').filter(|&at| at >= rendered) {
            self.lines.extend(
                text[rendered..end]
                    .split('\n')
                    .map(|line| self.style.row(line)),
            );
            self.rendered_to = ByteOffset(end + 1);
            self.stable_lines = LineCount(self.lines.len());
        }
        if self.rendered_to.0 < text.len() || self.lines.len() == 1 {
            self.lines
                .push(self.style.clone().row(&text[self.rendered_to.0..]));
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
        let lines = render_events(tail, tail_events.into_iter());
        let lines = separate(lines);
        self.live = Some((self.revision, lines));
        if commit_end == ByteOffset(0) {
            let complete = self.block.rfind('\n').map_or("", |at| &self.block[..at]);
            let candidate = complete.rfind('\n').map_or("", |at| &complete[..=at]);
            if !Fence::establish(candidate).is_some_and(|mut open| open.extend(candidate))
                && unstable_inline(candidate, &self.references)
            {
                return Vec::new();
            }
            let stable = separated_with(candidate, &self.references);
            let live = &self.live.as_ref().expect("set above").1;
            self.emitted_lines = self.emitted_lines.min(LineCount(live.len()));
            let end = LineCount(stable.iter().zip(live).take_while(|(a, b)| a == b).count());
            let end = end.max(self.emitted_lines);
            let lines = live[self.emitted_lines.0..end.0].to_vec();
            self.emitted_lines = end;
            return lines;
        }
        let finished = self.block[..commit_end.0].to_owned();
        self.block = Clean::new(&self.block[commit_end.0..]);
        self.layout_bytes = self.block.len();
        separated_with(&finished, &self.references)
            .into_iter()
            .skip(std::mem::take(&mut self.emitted_lines).0)
            .collect()
    }
    pub(super) fn finish(&mut self) -> Vec<RenderRow> {
        let lines = separated_with(&self.block, &self.references)
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
            self.live = Some((self.revision, separated_with(&self.block, &self.references)));
        }
        let lines = &self.live.as_ref().expect("prepared above").1;
        &lines[self.emitted_lines.0.min(lines.len())..]
    }
}

fn unstable_inline(text: &str, references: &References) -> bool {
    enum Context {
        Prose,
        Code,
    }
    if text.contains("[^") || latex_delimiters(text).1.is_some() {
        return true;
    }
    let mut context = Context::Prose;
    parser(text, text, references).any(|event| match event {
        Event::Start(Tag::Table(_)) => true,
        Event::Start(Tag::CodeBlock(_)) => {
            context = Context::Code;
            false
        }
        Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
            context = Context::Prose;
            false
        }
        Event::Text(_) if matches!(context, Context::Code) => false,
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

fn separated_with(text: &str, references: &References) -> Vec<RenderRow> {
    separate(render_filtered(text, references))
}

fn separate(lines: Vec<RenderRow>) -> Vec<RenderRow> {
    if lines.is_empty() {
        return lines;
    }
    std::iter::once(RenderRow::literal(Line::default()))
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
