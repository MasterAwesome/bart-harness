use crate::tui::{text, transcript::DetailSequence};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::{borrow::Borrow, ops::Range, sync::Arc};
use unicode_segmentation::UnicodeSegmentation as _;

pub(super) type Point = (u64, usize);
pub(super) type Anchor = (u64, usize, usize);

#[derive(Clone)]
pub(super) struct Row {
    pub(super) anchor: Anchor,
    line: Line<'static>,
    content: Content,
    detail: Option<DetailSequence>,
}

#[derive(Clone)]
enum Content {
    Text {
        part: Part,
        skipped: String,
    },
    Table {
        source: Arc<Table>,
        boxes: CellBoxes,
    },
    Chrome(Point),
    Notice(&'static str),
}

#[derive(Clone)]
struct Part {
    byte: usize,
    spans: Range<usize>,
}

#[derive(Clone)]
pub(super) struct CellBox {
    right: usize,
    cell: usize,
    part: Part,
}

#[derive(Clone)]
struct CellBoxes {
    leading: Vec<CellBox>,
    last: CellBox,
}

impl CellBoxes {
    fn iter(&self) -> impl Iterator<Item = &CellBox> {
        self.leading.iter().chain(std::iter::once(&self.last))
    }

    fn hit(&self, column: usize) -> &Part {
        &self
            .leading
            .iter()
            .find(|hit| column < hit.right)
            .unwrap_or(&self.last)
            .part
    }
}

impl CellBox {
    pub(super) fn new(right: usize, cell: usize, byte: usize, spans: Range<usize>) -> Self {
        Self {
            right,
            cell,
            part: Part { byte, spans },
        }
    }
}

pub(super) struct Table {
    start: usize,
    cells: Vec<Cell>,
    separator: Option<String>,
}

struct Cell {
    byte: usize,
    text: String,
}

impl Cell {
    fn grapheme(&self, offset: usize) -> Option<&str> {
        if offset == self.text.len() {
            Some("")
        } else {
            self.text
                .grapheme_indices(true)
                .take_while(|(byte, _)| *byte <= offset)
                .find_map(|(byte, glyph)| (byte == offset).then_some(glyph))
        }
    }
}

impl Table {
    pub(super) fn new(
        start: usize,
        cells: impl IntoIterator<Item = String>,
        separator: Option<String>,
    ) -> Self {
        let cells = cells
            .into_iter()
            .scan(start + 1, |byte, text| {
                let cell = Cell { byte: *byte, text };
                *byte += cell.text.len() + 1;
                Some(cell)
            })
            .collect();
        Self {
            start,
            cells,
            separator,
        }
    }
    pub(super) fn cell(&self, index: usize) -> (usize, usize) {
        let cell = &self.cells[index];
        (cell.byte, cell.byte + cell.text.len())
    }
    pub(super) fn end(&self) -> usize {
        self.cells
            .last()
            .map_or(self.start, |cell| cell.byte + cell.text.len())
    }
    pub(super) fn markdown(&self, separator: bool) -> String {
        let mut row = String::from("|");
        for cell in &self.cells {
            row.push(' ');
            escape_cell(&mut row, &cell.text);
            row.push_str(" |");
        }
        if let Some(rule) = self.separator.as_ref().filter(|_| separator) {
            row.push('\n');
            row.push_str(rule);
        }
        row
    }
    fn copy_scope(&self, block: u64, (first, last): (Point, Point)) -> Option<TableScope<'_>> {
        if last <= (block, self.start) || first > (block, self.end()) {
            return None;
        }
        if first.0 == block && last.0 == block {
            for cell in &self.cells {
                let end = cell.byte + cell.text.len();
                if first.1 >= cell.byte && last.1 <= end {
                    return Some(TableScope::Cell(
                        cell,
                        first.1 - cell.byte,
                        last.1 - cell.byte,
                    ));
                }
            }
        }
        Some(TableScope::Row {
            separator: last > (block, self.end()),
        })
    }
}

fn escape_cell(out: &mut String, cell: &str) {
    for segment in cell.split_inclusive('|') {
        let before_pipe = segment.strip_suffix('|');
        let literal = before_pipe.map_or(segment, |text| text.trim_end_matches('\\'));
        let mut chars = literal.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\n' => out.push(' '),
                '&' => out.push_str("&amp;"),
                '\\' if chars.peek().is_some_and(char::is_ascii_punctuation) => {
                    out.push_str("\\\\");
                }
                '*' | '_' | '~' | '`' | '[' | ']' | '<' | '>' | '$' => {
                    out.push('\\');
                    out.push(ch);
                }
                _ => out.push(ch),
            }
        }
        if let Some(text) = before_pipe {
            for _ in literal.len()..text.len() {
                out.push_str("&#92;");
            }
            out.push_str("\\|");
        }
    }
}

enum TableScope<'a> {
    Cell(&'a Cell, usize, usize),
    Row { separator: bool },
}

impl Row {
    pub(super) fn line(&self) -> &Line<'static> {
        &self.line
    }
    pub(super) fn detail(&self) -> Option<DetailSequence> {
        self.detail
    }
    pub(super) fn with_detail(mut self, detail: Option<DetailSequence>) -> Self {
        self.detail = detail;
        self
    }
    #[cfg(test)]
    pub(super) fn into_line(self) -> Line<'static> {
        self.line
    }
    pub(super) fn chrome(anchor: Anchor, line: Line<'static>) -> Self {
        Self::chrome_at(anchor, (anchor.0, anchor.1), line)
    }
    pub(super) fn chrome_at(anchor: Anchor, point: Point, line: Line<'static>) -> Self {
        Self {
            anchor,
            line,
            content: Content::Chrome(point),
            detail: None,
        }
    }
    pub(super) fn notice(anchor: Anchor, line: Line<'static>, source: &'static str) -> Self {
        Self {
            anchor,
            line,
            content: Content::Notice(source),
            detail: None,
        }
    }
    pub(super) fn table(
        anchor: Anchor,
        mut line: Line<'static>,
        prefix: &[Span<'static>],
        source: &Arc<Table>,
        mut boxes: Vec<CellBox>,
    ) -> Self {
        let width = prefix.iter().map(Span::width).sum::<usize>();
        line.spans.splice(..0, prefix.iter().cloned());
        for cell in &mut boxes {
            cell.right += width;
            cell.part.spans.start += prefix.len();
            cell.part.spans.end += prefix.len();
        }
        match boxes.pop() {
            Some(last) => Self {
                anchor,
                line,
                content: Content::Table {
                    source: Arc::clone(source),
                    boxes: CellBoxes {
                        leading: boxes,
                        last,
                    },
                },
                detail: None,
            },
            None => Self::chrome(anchor, line),
        }
    }
    pub(super) fn skipped(&self) -> &str {
        match &self.content {
            Content::Text { skipped, .. } => skipped,
            _ => "",
        }
    }
    fn segment(&self, part: &Part) -> (usize, &[Span<'static>]) {
        let (before, content) = self.line.spans.split_at(part.spans.start);
        (
            before.iter().map(|span| span.content.len()).sum(),
            &content[..part.spans.len()],
        )
    }
    pub(super) fn indent(&self) -> usize {
        match &self.content {
            Content::Text { part, .. } => self.column(part),
            _ => 0,
        }
    }
    fn column(&self, part: &Part) -> usize {
        self.line.spans[..part.spans.start]
            .iter()
            .map(Span::width)
            .sum()
    }
    fn graphemes(&self, part: &Part) -> impl Iterator<Item = (usize, usize, &str, Style)> {
        let mut byte = part.byte;
        let mut column = self.column(part);
        self.line.spans[part.spans.clone()]
            .iter()
            .flat_map(|span| span.content.graphemes(true).map(move |g| (g, span.style)))
            .map(move |(g, style)| {
                let at = (byte, column, g, style);
                byte += g.len();
                column += text::width(g);
                at
            })
    }
}

pub(super) fn graphemes(row: &Row) -> impl Iterator<Item = (usize, usize, &str, Style)> {
    match &row.content {
        Content::Text { part, .. } => Some(part),
        _ => None,
    }
    .into_iter()
    .flat_map(|part| row.graphemes(part))
}

pub(super) fn point_at(row: &Row, column: usize) -> Point {
    let part = match &row.content {
        Content::Text { part, .. } => part,
        Content::Table { boxes, .. } => boxes.hit(column),
        Content::Chrome(point) => return *point,
        Content::Notice(_) => return (row.anchor.0, row.anchor.1),
    };
    let byte = row
        .graphemes(part)
        .take_while(|(_, first, ..)| *first <= column)
        .last()
        .map_or(part.byte, |(byte, ..)| byte);
    (row.anchor.0, byte)
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

pub(super) fn highlight(row: &Row, range: (Point, Point)) -> Line<'_> {
    if matches!(row.content, Content::Notice(_))
        && range.0 <= (row.anchor.0, row.anchor.1)
        && (row.anchor.0, row.anchor.1) <= range.1
    {
        return borrowed(&row.line).style(row.line.style.add_modifier(Modifier::REVERSED));
    }
    let mut line = Line {
        spans: Vec::new(),
        style: row.line.style,
        alignment: row.line.alignment,
    };
    let mut span = 0;
    let mut part = |part: &Part, selected: &dyn Fn(usize) -> bool| {
        line.spans.extend(
            row.line.spans[span..part.spans.start]
                .iter()
                .map(|span| Span::styled(span.content.as_ref(), span.style)),
        );
        line.spans
            .extend(row.graphemes(part).map(|(byte, _, g, style)| {
                Span::styled(
                    g,
                    if selected(byte) {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    },
                )
            }));
        span = part.spans.end;
    };
    let block = row.anchor.0;
    match &row.content {
        Content::Text { part: text, .. } => part(text, &|byte| {
            range.0 <= (block, byte) && (block, byte) <= range.1
        }),
        Content::Table { source, boxes } => match source.copy_scope(block, range) {
            Some(TableScope::Row { .. }) => boxes.iter().for_each(|hit| part(&hit.part, &|_| true)),
            Some(TableScope::Cell(cell, first, last)) => boxes.iter().for_each(|hit| {
                let first = cell.byte + first;
                let last = cell.byte + last;
                if hit.part.byte <= last && hit.part.byte >= cell.byte {
                    part(&hit.part, &|byte| first <= byte && byte <= last);
                }
            }),
            None => {}
        },
        Content::Chrome(_) | Content::Notice(_) => {}
    }
    line.spans.extend(
        row.line.spans[span..]
            .iter()
            .map(|span| Span::styled(span.content.as_ref(), span.style)),
    );
    line
}

#[derive(PartialEq, Eq)]
enum Target<'a> {
    Glyph(&'a str),
    Cell(usize, &'a str),
    EmptyLine,
    Chrome,
    Notice(&'static str),
}

fn target(rows: &[Row], (block, byte): Point) -> Option<Target<'_>> {
    let mut previous_table = None;
    rows.iter()
        .filter(|row| row.anchor.0 == block)
        .find_map(|row| match &row.content {
            Content::Text { part, skipped } => {
                let (_, spans) = row.segment(part);
                let len: usize = spans.iter().map(|span| span.content.len()).sum();
                if len == 0 && byte == part.byte {
                    return Some(Target::EmptyLine);
                }
                row.graphemes(part)
                    .find_map(|(at, _, glyph, _)| (at == byte).then_some(Target::Glyph(glyph)))
                    .or_else(|| {
                        skipped.grapheme_indices(true).find_map(|(at, glyph)| {
                            (part.byte + len + at == byte).then_some(Target::Glyph(glyph))
                        })
                    })
            }
            Content::Table { source, .. } => {
                if previous_table
                    .replace(source)
                    .is_some_and(|prior| Arc::ptr_eq(prior, source))
                {
                    return None;
                }
                source.cells.iter().enumerate().find_map(|(index, cell)| {
                    let offset = byte.checked_sub(cell.byte)?;
                    (offset <= cell.text.len()).then_some(())?;
                    cell.grapheme(offset)
                        .map(|glyph| Target::Cell(index, glyph))
                })
            }
            Content::Chrome(point) => (*point == (block, byte)).then_some(Target::Chrome),
            Content::Notice(notice) => (row.anchor.1 == byte).then_some(Target::Notice(notice)),
        })
}

pub(super) fn unchanged(old: &[Row], new: &[Row], a: Option<Point>, b: Option<Point>) -> bool {
    let same = |point| target(old, point).is_some_and(|prior| target(new, point) == Some(prior));
    match (a, b) {
        (Some(a), Some(b)) => {
            let range = (a.min(b), a.max(b));
            same(a) && same(b) && text(old, range) == text(new, range)
        }
        (Some(point), None) | (None, Some(point)) => {
            let range = ((point.0, 0), point);
            same(point) && text(old, range) == text(new, range)
        }
        (None, None) => true,
    }
}

pub(super) fn text(
    rows: impl IntoIterator<Item = impl Borrow<Row>>,
    range: (Point, Point),
) -> Option<String> {
    let mut output = String::new();
    let mut previous_end = None;
    for row in rows {
        let row = row.borrow();
        let block = row.anchor.0;
        match &row.content {
            Content::Chrome(_) => {}
            Content::Notice(notice) => {
                let point = (block, row.anchor.1);
                if range.0 <= point && point <= range.1 {
                    if previous_end.is_some() {
                        output.push('\n');
                    }
                    output.push_str(notice);
                    previous_end = Some(point);
                }
            }
            Content::Table { source, .. } => {
                let end = (block, source.end());
                if previous_end == Some(end) {
                    continue;
                }
                match source.copy_scope(block, range) {
                    Some(TableScope::Cell(cell, first, last)) => {
                        cell.grapheme(first)?;
                        let stop = last + cell.grapheme(last)?.len();
                        output.push_str(cell.text.get(first..stop)?);
                    }
                    Some(TableScope::Row { separator }) => {
                        if previous_end.is_some() {
                            output.push('\n');
                        }
                        output.push_str(&source.markdown(separator));
                    }
                    None => continue,
                }
                previous_end = Some(end);
            }
            Content::Text { part, skipped } => {
                let byte = part.byte;
                let len: usize = row.line.spans[part.spans.clone()]
                    .iter()
                    .map(|span| span.content.len())
                    .sum();
                let end = byte + len + skipped.len();
                if block < range.0.0
                    || block > range.1.0
                    || block == range.0.0 && end <= range.0.1 && (end != byte || byte < range.0.1)
                    || (block, byte) > range.1
                {
                    continue;
                }
                if previous_end.is_some_and(|(id, end)| id != block || byte > end) {
                    output.push('\n');
                }
                output.extend(
                    row.graphemes(part)
                        .filter(|(byte, ..)| range.0 <= (block, *byte) && (block, *byte) <= range.1)
                        .map(|(_, _, g, _)| g),
                );
                output.extend(
                    skipped
                        .grapheme_indices(true)
                        .filter(|(offset, _)| {
                            range.0 <= (block, byte + len + offset)
                                && (block, byte + len + offset) <= range.1
                        })
                        .map(|(_, g)| g),
                );
                previous_end = Some((block, end));
            }
        }
    }
    (!output.is_empty()).then_some(output)
}

#[derive(Clone)]
struct Fragment {
    row: usize,
    source: Range<usize>,
}

#[derive(Clone, Copy)]
pub(super) struct Hit {
    pub(super) anchor: Anchor,
    byte: usize,
}

impl Hit {
    pub(super) fn start(self) -> Point {
        (self.anchor.0, self.byte)
    }
}

fn record(
    hits: &mut Vec<Hit>,
    rows: &[Row],
    text: &str,
    base: usize,
    query: &str,
    fragments: &[Fragment],
) {
    let mut visible = fragments
        .iter()
        .filter(|part| !part.source.is_empty())
        .peekable();
    for (offset, needle) in text.to_ascii_lowercase().match_indices(query) {
        let start = base + offset;
        while visible.peek().is_some_and(|part| part.source.end <= start) {
            visible.next();
        }
        let Some(part) = visible.peek() else { break };
        if part.source.start < start + needle.len() {
            let anchor = rows[part.row].anchor;
            hits.push(Hit {
                anchor,
                byte: start,
            });
        }
    }
}

fn table_hits(hits: &mut Vec<Hit>, rows: &[Row], source: &Table, range: Range<usize>, query: &str) {
    let mut cells = vec![Vec::new(); source.cells.len()];
    for index in range {
        let row = &rows[index];
        if let Content::Table { boxes, .. } = &row.content {
            for hit in boxes.iter() {
                let (_, spans) = row.segment(&hit.part);
                let len: usize = spans.iter().map(|span| span.content.len()).sum();
                cells[hit.cell].push(Fragment {
                    row: index,
                    source: hit.part.byte..hit.part.byte + len,
                });
            }
        }
    }
    for (cell, fragments) in source.cells.iter().zip(cells) {
        record(hits, rows, &cell.text, cell.byte, query, &fragments);
    }
}

// Keep the hits in source order. The visible order of table cells can differ.
pub(super) fn search(rows: &[Row], query: &str) -> Vec<Hit> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut index = 0;
    let mut text = String::new();
    let mut fragments = Vec::new();
    while let Some(row) = rows.get(index) {
        match &row.content {
            Content::Text { part, .. } => {
                let start = part.byte;
                let block = row.anchor.0;
                while let Some(next) = rows.get(index).filter(|row| row.anchor.0 == block) {
                    let Content::Text { part, skipped } = &next.content else {
                        break;
                    };
                    if part.byte != start + text.len() {
                        break;
                    }
                    let (_, spans) = next.segment(part);
                    text.extend(spans.iter().map(|span| span.content.as_ref()));
                    fragments.push(Fragment {
                        row: index,
                        source: part.byte..start + text.len(),
                    });
                    text.push_str(skipped);
                    index += 1;
                }
                record(&mut hits, rows, &text, start, query, &fragments);
                text.clear();
                fragments.clear();
            }
            Content::Table { source, .. } => {
                let len = rows[index..].iter().take_while(|row| matches!(&row.content, Content::Table { source: next, .. } if Arc::ptr_eq(next, source))).count();
                table_hits(&mut hits, rows, source, index..index + len, query);
                index += len;
            }
            Content::Chrome(_) | Content::Notice(_) => index += 1,
        }
    }
    hits
}

pub(super) fn marks(row: &Row, hits: &[Hit], bytes: usize) -> Vec<Range<usize>> {
    if hits.is_empty() {
        return Vec::new();
    }
    let mut marks = Vec::new();
    let mut mark = |part: &Part| {
        let (display, spans) = row.segment(part);
        let block = row.anchor.0;
        let end = part.byte + spans.iter().map(|span| span.content.len()).sum::<usize>();
        if end == part.byte {
            return;
        }
        let first =
            hits.partition_point(|hit| (hit.anchor.0, hit.byte + bytes) <= (block, part.byte));
        marks.extend(
            hits[first..]
                .iter()
                .take_while(|hit| hit.start() < (block, end))
                .map(|hit| {
                    let start = display + hit.byte.saturating_sub(part.byte);
                    let end = display + (hit.byte + bytes).min(end) - part.byte;
                    start..end
                }),
        );
    };
    match &row.content {
        Content::Text { part, .. } => mark(part),
        Content::Table { boxes, .. } => boxes.iter().for_each(|cell| mark(&cell.part)),
        Content::Chrome(_) | Content::Notice(_) => {}
    }
    marks
}

pub(super) fn wrap_line(line: &Line<'static>, block: u64, byte: usize, width: usize) -> Vec<Row> {
    wrap_prefixed(line, &[], &[], (block, byte), width, text::Wrap::Prose)
}

pub(super) fn fit_prefix<'a>(
    prefix: &'a [Span<'static>],
    width: usize,
    room: usize,
) -> &'a [Span<'static>] {
    const COLLAPSED: &[Span<'static>] = &[Span {
        content: std::borrow::Cow::Borrowed("…"),
        style: Style::new().dim(),
    }];
    let spare = width.saturating_sub(room);
    if prefix.iter().map(Span::width).sum::<usize>() <= spare {
        prefix
    } else if spare > 0 {
        COLLAPSED
    } else {
        &[]
    }
}

pub(super) fn wrap_prefixed(
    line: &Line<'static>,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    (block, byte): Point,
    width: usize,
    mode: text::Wrap,
) -> Vec<Row> {
    let room = if first.is_empty() && rest.is_empty() {
        1
    } else {
        text::glyph_room(&line.spans, width)
    };
    let prefixes = [first, rest].map(|prefix| fit_prefix(prefix, width, room));
    let widths = prefixes.map(|prefix| width - prefix.iter().map(Span::width).sum::<usize>());
    text::wrap(&line.spans, widths, usize::MAX, mode)
        .0
        .into_iter()
        .enumerate()
        .map(|(index, wrapped)| {
            let prefix = prefixes[usize::from(index != 0)];
            let mut spans = prefix.to_vec();
            spans.extend(wrapped.line.spans);
            let byte = byte + wrapped.byte;
            let content = Content::Text {
                part: Part {
                    byte,
                    spans: prefix.len() + wrapped.padding..spans.len(),
                },
                skipped: wrapped.skipped,
            };
            Row {
                anchor: (block, byte, 0),
                line: Line {
                    spans,
                    style: line.style,
                    alignment: line.alignment,
                },
                content,
                detail: None,
            }
        })
        .collect()
}
