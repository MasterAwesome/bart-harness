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

#[derive(Clone, Copy)]
pub(super) enum Wrap {
    Prose,
    Table,
}

pub(super) fn wrap(
    spans: &[Span<'static>],
    width: usize,
    limit: usize,
    mode: Wrap,
) -> (Vec<(Line<'static>, usize)>, bool) {
    if limit == 0 || width == 0 {
        return (Vec::new(), !spans.is_empty());
    }
    let indent = if matches!(mode, Wrap::Prose) {
        spans
            .iter()
            .flat_map(|s| s.content.graphemes(true))
            .take_while(|g| !g.chars().any(char::is_alphanumeric) && *g != "\n")
            .map(str::width)
            .sum::<usize>()
            .min(width / 2)
    } else {
        0
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
    let mut rows = vec![(Line::default(), 0)];
    let mut used = 0;
    let mut at = 0;
    let mut style = 0;
    let mut word_end = 0;
    let mut split_word = false;
    while at < source.len() {
        let rest = &source[at..];
        let first = rest.chars().next().expect("non-empty source");
        let whitespace = first.is_whitespace();
        if !whitespace && at >= word_end {
            word_end = at + rest.find(char::is_whitespace).unwrap_or(rest.len());
            split_word = source[at..word_end].width() > width.saturating_sub(indent);
        }
        let mut len = first.len_utf8();
        let mut cells = source[at..at + len].width();
        if !whitespace {
            len = word_end - at;
            if split_word {
                len = 0;
                cells = 0;
                for grapheme in source[at..word_end].graphemes(true) {
                    let next = grapheme.width();
                    if len > 0 && cells + next > width.saturating_sub(used) {
                        break;
                    }
                    len += grapheme.len();
                    cells += next;
                    if cells >= width.saturating_sub(used) {
                        break;
                    }
                }
            } else {
                cells = source[at..word_end].width();
            }
        }
        if matches!(mode, Wrap::Table)
            && whitespace
            && first != '\n'
            && (used == 0 || used >= width)
        {
            at += len;
            continue;
        }
        let start = rows.last().expect("one row").1;
        let newline = first == '\n';
        if newline || used > start && used + cells > width && !whitespace {
            if rows.len() == limit {
                return (rows, true);
            }
            let pad = if newline {
                0
            } else {
                indent.min(width.saturating_sub(cells))
            };
            let mut fill = Vec::new();
            let mut left = pad;
            for span in spans {
                let cells = span.content.width().min(left);
                if cells == 0 {
                    break;
                }
                let bg = span.style.bg;
                fill.push(Span::styled(
                    " ".repeat(cells),
                    Style { bg, ..Style::new() },
                ));
                left -= cells;
            }
            rows.push((Line::from(fill), pad));
            used = pad;
        }
        let end = at + len;
        if newline {
            at = end;
            continue;
        }
        while at < end {
            while styles[style].0 <= at {
                style += 1;
            }
            let to = end.min(styles[style].0);
            let (line, pad) = rows.last_mut().expect("one row");
            if line.spans.len() > usize::from(*pad > 0)
                && let Some(last) = line.spans.last_mut()
                && last.style == styles[style].1
            {
                last.content.to_mut().push_str(&source[at..to]);
            } else {
                line.spans
                    .push(Span::styled(source[at..to].to_owned(), styles[style].1));
            }
            at = to;
        }
        used += cells;
    }
    (rows, false)
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
