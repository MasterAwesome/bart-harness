use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Clean(String);

impl Clean {
    pub(super) fn new(input: &str) -> Self {
        let mut text = Self::default();
        text.append(input, &mut crate::terminal_text::Display::default());
        text
    }
    pub(super) fn literal(text: &'static str) -> Self {
        Self(text.to_owned())
    }
    pub(super) fn append(&mut self, input: &str, filter: &mut crate::terminal_text::Display) {
        let start = self.0.len();
        filter.push(input, |part| self.0.push_str(part));
        if !self.0[start..].contains('\t') {
            return;
        }
        let start = self.0[..start].rfind('\n').map_or(0, |at| at + 1);
        let mut expanded = String::new();
        let mut column = 0;
        for line in self.0[start..].split_inclusive('\n') {
            for (index, part) in line.split('\t').enumerate() {
                if index > 0 {
                    let pad = 4 - column % 4;
                    expanded.push_str(&"    "[..pad]);
                    column += pad;
                }
                expanded.push_str(part);
                column += width(part);
            }
            if line.ends_with('\n') {
                column = 0;
            }
        }
        self.0.truncate(start);
        self.0.push_str(&expanded);
    }
}
impl std::ops::Deref for Clean {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for Clean {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl From<Clean> for std::borrow::Cow<'static, str> {
    fn from(text: Clean) -> Self {
        Self::Owned(text.0)
    }
}
impl From<Clean> for Line<'static> {
    fn from(text: Clean) -> Self {
        Self::from(Span::raw(text))
    }
}
impl From<Clean> for ratatui::text::Text<'static> {
    fn from(text: Clean) -> Self {
        Self::from(Line::from(text))
    }
}

pub(super) fn width(text: &str) -> usize {
    text.width()
}

pub(super) fn glyph_room(spans: &[Span<'_>], width: usize) -> usize {
    let source: String = spans.iter().map(|span| span.content.as_ref()).collect();
    source
        .graphemes(true)
        .map(str::width)
        .max()
        .unwrap_or(1)
        .clamp(1, width.max(1))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Wrap {
    Prose,
    Plain,
    Table,
    Verbatim,
}

#[derive(Default)]
pub(super) struct Wrapped {
    pub(super) line: Line<'static>,
    pub(super) padding: usize,
    pub(super) byte: usize,
    pub(super) skipped: String,
}

pub(super) fn wrap(
    spans: &[Span<'static>],
    widths: [usize; 2],
    limit: usize,
    mode: Wrap,
) -> (Vec<Wrapped>, bool) {
    if limit == 0 {
        return (Vec::new(), !spans.is_empty());
    }
    let annotated = super::links::annotated(spans);
    let spans = annotated.as_deref().unwrap_or(spans);
    let [mut width, continuation] = widths.map(|width| width.max(1));
    let indent = match mode {
        Wrap::Prose => prose_indent(spans).min(continuation / 2),
        _ => 0,
    };
    let source: String = spans.iter().map(|span| span.content.as_ref()).collect();
    let mut offset = 0;
    let styles: Vec<_> = spans
        .iter()
        .map(|span| {
            offset += span.content.len();
            (offset, span.style)
        })
        .collect();
    let mut rows = vec![Wrapped::default()];
    let mut used = 0;
    let mut pad = 0;
    let mut style = 0;
    let mut word_end = 0;
    let mut word_width = 0;
    let mut split_word = false;
    let mut has_word = false;
    let mut graphemes = source.grapheme_indices(true).peekable();
    while let Some(&(start, grapheme)) = graphemes.peek() {
        let rest = &source[start..];
        let newline = grapheme == "\n";
        let whitespace = grapheme.starts_with(char::is_whitespace);
        let verbatim = matches!(mode, Wrap::Verbatim);
        if !whitespace && !verbatim && start >= word_end {
            word_end = start + rest.find(char::is_whitespace).unwrap_or(rest.len());
            word_width = source[start..word_end].width();
            split_word = word_width > width.min(continuation.saturating_sub(indent));
        }
        let (end, cells) = if whitespace || verbatim || split_word {
            (start + grapheme.len(), grapheme.width())
        } else {
            (word_end.max(start + grapheme.len()), word_width)
        };
        if matches!(mode, Wrap::Table) && whitespace && !newline && (used == 0 || used >= width) {
            graphemes.next();
            continue;
        }
        let row = rows.last_mut().expect("one row");
        let has_skipped = !row.skipped.is_empty();
        if matches!(mode, Wrap::Prose | Wrap::Plain)
            && has_word
            && whitespace
            && !newline
            && (used + cells > width || has_skipped)
        {
            row.skipped.push_str(grapheme);
            graphemes.next();
            continue;
        }
        if newline || has_skipped || used > pad && used + cells > width {
            if rows.len() == limit {
                return (rows, true);
            }
            width = continuation;
            pad = if newline {
                0
            } else {
                indent.min(width.saturating_sub(cells))
            };
            let fill = continuation_padding(spans, pad);
            rows.push(Wrapped {
                padding: fill.len(),
                line: Line::from(fill),
                byte: if newline { end } else { start },
                ..Wrapped::default()
            });
            used = pad;
            has_word = false;
        }
        if newline {
            graphemes.next();
            continue;
        }
        let row = rows.last_mut().expect("one row");
        if row.line.spans.len() == row.padding {
            row.byte = start;
        }
        while graphemes.peek().is_some_and(|(next, _)| *next < end) {
            let (at, grapheme) = graphemes.next().expect("peeked grapheme");
            while styles[style].0 <= at {
                style += 1;
            }
            row.push(grapheme, styles[style].1);
        }
        used += cells;
        has_word |= !whitespace;
    }
    (rows, false)
}

impl Wrapped {
    fn push(&mut self, grapheme: &str, style: Style) {
        let spans = &mut self.line.spans;
        if spans.len() > self.padding
            && let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(grapheme);
        } else {
            spans.push(Span::styled(grapheme.to_owned(), style));
        }
    }
}

fn prose_indent(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .flat_map(|span| span.content.graphemes(true))
        .take_while(|g| !g.chars().any(char::is_alphanumeric) && *g != "\n")
        .map(str::width)
        .sum()
}

fn continuation_padding(spans: &[Span<'static>], mut left: usize) -> Vec<Span<'static>> {
    let mut fill = Vec::new();
    for span in spans {
        if left == 0 {
            break;
        }
        let cells = span.width().min(left);
        if cells > 0 {
            fill.push(Span::styled(
                " ".repeat(cells),
                Style {
                    bg: span.style.bg,
                    ..Style::new()
                },
            ));
            left -= cells;
        }
    }
    fill
}

pub(super) fn clip_end(text: &str, room: usize) -> String {
    if width(text) <= room {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let size = width(grapheme);
        if used + size > room.saturating_sub(1) {
            break;
        }
        out.push_str(grapheme);
        used += size;
    }
    if room > 0 {
        out.push('…');
    }
    out
}

pub(super) fn clip_start(text: &str, room: usize) -> String {
    if width(text) <= room {
        return text.to_owned();
    }
    if room == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut start = text.len();
    for (at, grapheme) in text.grapheme_indices(true).rev() {
        let size = width(grapheme);
        if used + size > room - 1 {
            break;
        }
        start = at;
        used += size;
    }
    format!("…{}", &text[start..])
}

pub(super) struct Item {
    pub(super) full: String,
    pub(super) short: Option<String>,
    pub(super) priority: usize,
}

pub(super) fn fit(items: impl IntoIterator<Item = Item>, room: usize) -> String {
    let items: Vec<_> = items
        .into_iter()
        .map(|mut item| {
            item.full = item.full.replace(['\n', '\r'], " ");
            item.short = item.short.map(|text| text.replace(['\n', '\r'], " "));
            item
        })
        .filter(|item| !item.full.is_empty())
        .collect();
    let mut shown: Vec<_> = items.iter().map(|item| Some(item.full.as_str())).collect();
    let mut order: Vec<_> = (0..items.len()).collect();
    order.sort_by_key(|&i| (items[i].priority, std::cmp::Reverse(i)));
    let join = |shown: &[Option<&str>]| {
        shown
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .join(" · ")
    };
    for i in order {
        if width(&join(&shown)) <= room {
            break;
        }
        if let Some(short) = &items[i].short {
            shown[i] = Some(short);
            if width(&join(&shown)) <= room {
                break;
            }
        }
        shown[i] = None;
    }
    join(&shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_pads_continuations_past_empty_leading_span() {
        let spans = vec![
            Span::raw(""),
            Span::raw("  "),
            Span::raw("alpha beta gamma delta"),
        ];
        let (rows, _) = wrap(&spans, [10; 2], usize::MAX, Wrap::Prose);
        assert!(rows.len() >= 2, "expected wrapping into >= 2 rows");
        for row in &rows[1..] {
            let line = &row.line;
            assert!(
                line.to_string().starts_with("  "),
                "wrapped continuation missing indent padding: {:?}",
                line.to_string()
            );
        }
    }
}
