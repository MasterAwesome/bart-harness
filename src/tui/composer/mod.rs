use crate::tui::selection::{self, Point, Row};
use crate::tui::text::{self, Clean};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone, Default)]
pub(super) struct Composer {
    text: String,
    cursor: usize,
    width: usize,
    pastes: Vec<Fold>,
    layout: Option<DraftLayout>,
    selection: Option<(Point, Point, bool)>,
    dragging: bool,
    area: Rect,
    first: usize,
}

#[derive(Clone)]
struct DraftLayout {
    width: usize,
    rows: Vec<Row>,
}

#[derive(Clone)]
struct Fold {
    start: usize,
    end: usize,
    id: usize,
    content: std::sync::Arc<str>,
    saved: Option<super::paste::Reference>,
}

impl Composer {
    pub(super) fn append(&mut self, mut other: Self) {
        let offset = self.text.len();
        for paste in &mut other.pastes {
            paste.start += offset;
            paste.end += offset;
        }
        self.layout = None;
        self.clear_selection();
        self.text.push_str(&other.text);
        self.pastes.extend(other.pastes);
        self.end();
    }

    pub(super) fn content_bytes(&self) -> usize {
        self.text.len()
            - self
                .pastes
                .iter()
                .map(|paste| paste.end - paste.start)
                .sum::<usize>()
            + self.pasted_bytes()
    }

    pub(super) fn pasted_bytes(&self) -> usize {
        self.pastes.iter().map(|paste| paste.content.len()).sum()
    }

    pub(super) fn paste(&mut self, text: &str, id: usize) -> bool {
        let bytes = super::paste::normalized_bytes(text);
        if bytes > super::MAX_DRAFT_BYTES
            || self.content_bytes().saturating_add(bytes) > super::MAX_DRAFT_BYTES
        {
            return false;
        }
        let text = super::paste::normalize(text);
        if !super::paste::folds(&text) {
            return self.insert(&text);
        }
        let token = super::paste::composer_token(id, &text);
        let start = self.byte(self.cursor);
        let cursor = self.cursor + token.chars().count();
        self.insert_text(&token);
        self.pastes.push(Fold {
            start,
            end: start + token.len(),
            id,
            content: text.as_ref().into(),
            saved: None,
        });
        self.pastes.sort_by_key(|paste| paste.start);
        self.cursor = cursor;
        true
    }

    pub(super) fn needs_save(&self) -> bool {
        self.pastes.iter().any(|paste| paste.saved.is_none())
    }

    pub(super) fn save_pastes(
        &mut self,
        artifacts: &crate::tool::Artifacts,
    ) -> std::io::Result<()> {
        for paste in &mut self.pastes {
            if paste.saved.is_none() {
                let reference = super::paste::Reference {
                    id: paste.id,
                    chars: paste.content.chars().count(),
                };
                if let Err(error) = artifacts.save_paste(paste.id as u64, &paste.content)
                    && !std::fs::read(reference.file(artifacts.dir()))
                        .is_ok_and(|bytes| bytes == paste.content.as_bytes())
                {
                    return Err(error);
                }
                paste.saved = Some(reference);
            }
        }
        Ok(())
    }

    pub(super) fn paste_previews(&self) -> super::paste::Previews {
        self.pastes
            .iter()
            .map(|paste| (paste.id, super::paste::preview(paste.id, &paste.content)))
            .collect()
    }

    pub(super) fn text_with_references(&self) -> String {
        let mut text = self.text.clone();
        for paste in self.pastes.iter().rev() {
            let mut reference = paste.saved.as_ref().expect("paste saved").text();
            if paste.end < text.len() {
                reference.push('\n');
            }
            text.replace_range(paste.start..paste.end, &reference);
        }
        text
    }

    pub(super) fn restore(text: &str, artifacts: &crate::tool::Artifacts) -> std::io::Result<Self> {
        let mut composer = Self::default();
        let mut end = 0;
        for (range, reference) in super::paste::references(text) {
            let path = reference.file(artifacts.dir());
            let load = || {
                String::from_utf8(crate::persist::read_to(
                    &crate::persist::open(&path)?,
                    super::MAX_DRAFT_BYTES as u64,
                )?)
                .map_err(std::io::Error::other)
            };
            let bytes = load().map_err(|error: std::io::Error| {
                std::io::Error::other(format!("cannot read saved paste {}: {error}", reference.id))
            })?;
            let folds = composer.pastes.len();
            if !composer.insert(&text[end..range.start]) || !composer.paste(&bytes, reference.id) {
                return Err(std::io::Error::other("restored draft exceeds 1 MiB"));
            }
            if composer.pastes.len() == folds {
                return Err(std::io::Error::other(
                    "saved paste could not be restored as a fold",
                ));
            }
            let paste = composer.pastes.last_mut().expect("saved paste is folded");
            paste.saved = Some(reference);
            end = range.end;
        }
        if !composer.insert(&text[end..]) {
            return Err(std::io::Error::other("restored draft exceeds 1 MiB"));
        }
        Ok(composer)
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn has_meaningful_input(&self) -> bool {
        let mut end = 0;
        for paste in &self.pastes {
            if !self.text[end..paste.start].trim().is_empty() || !paste.content.trim().is_empty() {
                return true;
            }
            end = paste.end;
        }
        !self.text[end..].trim().is_empty()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(index, _)| index)
    }

    pub(super) fn insert(&mut self, text: &str) -> bool {
        let mut clean;
        let text = if text
            .chars()
            .any(|c| c != '\n' && crate::terminal_text::unsafe_character(c))
        {
            clean = Clean::new(&self.text[..self.byte(self.cursor)]);
            let start = clean.len();
            clean.append(text, &mut crate::terminal_text::Display::default());
            &clean[start..]
        } else {
            text
        };
        if self.content_bytes().saturating_add(text.len()) > super::MAX_DRAFT_BYTES {
            return false;
        }
        self.insert_text(text);
        true
    }

    fn insert_text(&mut self, text: &str) {
        let at = self.byte(self.cursor);
        for paste in &mut self.pastes {
            if paste.start >= at {
                paste.start += text.len();
                paste.end += text.len();
            }
        }
        self.layout = None;
        self.clear_selection();
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
        let cursor = self
            .boundaries()
            .find(|&at| at >= self.cursor)
            .unwrap_or(self.cursor);
        self.cursor = cursor;
    }

    fn boundaries(&self) -> impl Iterator<Item = usize> + '_ {
        let mut segments = Vec::with_capacity(self.pastes.len() * 2 + 1);
        let mut end = 0;
        for paste in &self.pastes {
            segments.push((&self.text[end..paste.start], false));
            segments.push((&self.text[paste.start..paste.end], true));
            end = paste.end;
        }
        segments.push((&self.text[end..], false));
        let units = segments.into_iter().flat_map(|(text, folded)| {
            std::iter::once(text).take(usize::from(folded)).chain(
                text.graphemes(true)
                    .take(if folded { 0 } else { usize::MAX }),
            )
        });
        std::iter::once(0).chain(units.scan(0, |chars, unit| {
            *chars += unit.chars().count();
            Some(*chars)
        }))
    }

    pub(super) fn mention(&self) -> Option<&str> {
        let before = &self.text[..self.byte(self.cursor)];
        let word = &before[before
            .char_indices()
            .rfind(|(_, c)| c.is_whitespace())
            .map_or(0, |(at, c)| at + c.len_utf8())..];
        word.strip_prefix('@')
    }

    pub(super) fn complete_mention(&mut self, path: &str) -> bool {
        let Some(query) = self.mention() else {
            return true;
        };
        let replaced = query.len() + 1;
        if (self.content_bytes() - replaced).saturating_add(path.len() + 1) > super::MAX_DRAFT_BYTES
        {
            return false;
        }
        let start = self.cursor - query.chars().count() - 1;
        self.remove(start, self.cursor);
        self.insert(&format!("{path} "))
    }

    pub(super) fn backspace(&mut self) {
        let start = self
            .boundaries()
            .take_while(|&at| at < self.cursor)
            .last()
            .unwrap_or(0);
        self.remove(start, self.cursor);
    }

    pub(super) fn delete(&mut self) {
        let end = self
            .boundaries()
            .find(|&at| at > self.cursor)
            .unwrap_or(self.cursor);
        self.remove(self.cursor, end);
    }

    pub(super) fn delete_to_space(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.remove(start, self.cursor);
    }

    pub(super) fn delete_word_left(&mut self) {
        let start = self.word_left_of(self.cursor);
        self.remove(start, self.cursor);
    }

    pub(super) fn delete_word_right(&mut self) {
        let end = self.word_right_of(self.cursor);
        self.remove(self.cursor, end);
    }

    pub(super) fn delete_to_end(&mut self) {
        self.remove(self.cursor, self.text.chars().count());
    }

    pub(super) fn delete_to_start(&mut self) {
        self.remove(0, self.cursor);
    }

    pub(super) fn word_left(&mut self) {
        let target = self.word_left_of(self.cursor);
        let cursor = self
            .boundaries()
            .take_while(|&at| at <= target)
            .last()
            .unwrap_or(0);
        self.cursor = cursor;
    }

    pub(super) fn word_right(&mut self) {
        let target = self.word_right_of(self.cursor);
        let cursor = self.boundaries().find(|&at| at >= target).unwrap_or(target);
        self.cursor = cursor;
    }

    fn word_left_of(&self, mut at: usize) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        while at > 0 && !chars[at - 1].is_alphanumeric() {
            at -= 1;
        }
        while at > 0 && chars[at - 1].is_alphanumeric() {
            at -= 1;
        }
        at
    }

    fn word_right_of(&self, mut at: usize) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        while at < chars.len() && !chars[at].is_alphanumeric() {
            at += 1;
        }
        while at < chars.len() && chars[at].is_alphanumeric() {
            at += 1;
        }
        at
    }

    fn remove(&mut self, start: usize, end: usize) {
        let start = self
            .boundaries()
            .take_while(|&at| at <= start)
            .last()
            .unwrap_or(0);
        let end = self.boundaries().find(|&at| at >= end).unwrap_or(end);
        let (from, to) = (self.byte(start), self.byte(end));
        self.pastes.retain_mut(|paste| {
            if paste.start < to && paste.end > from {
                return false;
            }
            if paste.start >= to {
                paste.start -= to - from;
                paste.end -= to - from;
            }
            true
        });
        self.layout = None;
        self.clear_selection();
        self.text.replace_range(from..to, "");
        self.cursor = start;
    }

    pub(super) fn left(&mut self) {
        let cursor = self
            .boundaries()
            .take_while(|&at| at < self.cursor)
            .last()
            .unwrap_or(0);
        self.cursor = cursor;
    }

    pub(super) fn right(&mut self) {
        let cursor = self
            .boundaries()
            .find(|&at| at > self.cursor)
            .unwrap_or(self.cursor);
        self.cursor = cursor;
    }

    pub(super) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(super) fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    pub(super) fn line_up(&mut self) {
        self.vertical(-1);
    }

    pub(super) fn line_down(&mut self) {
        self.vertical(1);
    }

    fn visual_width(&self) -> usize {
        if self.width == 0 {
            usize::MAX
        } else {
            self.width
        }
    }

    fn visual_position(&mut self) -> (usize, usize) {
        self.cache_rows();
        self.cached().position(&self.text, self.cursor)
    }

    fn vertical(&mut self, direction: isize) {
        let (row, column) = self.visual_position();
        let Some(target_row) = row.checked_add_signed(direction) else {
            return;
        };
        let mut target = None;
        self.cached().walk(&self.text, |index, at, col, _| {
            if at == target_row && (target.is_none() || col <= column) {
                target = Some(index);
            }
        });
        if let Some(target) = target {
            self.cursor = self
                .boundaries()
                .take_while(|&at| at <= target)
                .last()
                .unwrap_or(0);
        }
    }

    pub(super) fn on_first_line(&mut self) -> bool {
        self.visual_position().0 == 0
    }

    pub(super) fn on_last_line(&mut self) -> bool {
        self.visual_position().0
            == self
                .cached()
                .position(&self.text, self.text.chars().count())
                .0
    }

    fn cached(&self) -> &DraftLayout {
        self.layout.as_ref().expect("layout populated")
    }

    fn cache_rows(&mut self) {
        let width = self.visual_width();
        if self
            .layout
            .as_ref()
            .is_none_or(|cached| cached.width != width)
        {
            let rows = selection::wrap_line(Line::from(Span::raw(self.text.clone())), 0, 0, width);
            self.layout = Some(DraftLayout { width, rows });
        }
    }

    pub(super) fn layout(&mut self, width: usize) -> (&[Row], (usize, usize)) {
        self.width = width.max(1);
        let at = self.visual_position();
        (&self.cached().rows, at)
    }

    pub(super) fn painted(&mut self, area: Rect, first: usize) {
        self.area = area;
        self.first = first;
    }

    pub(super) fn contains(&self, column: u16, row: u16) -> bool {
        self.area.contains((column, row).into())
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.dragging = false;
    }

    pub(super) fn selecting(&self) -> bool {
        self.dragging
    }

    pub(super) fn selected_range(&self) -> Option<(Point, Point)> {
        self.selection
            .filter(|(_, _, moved)| *moved)
            .map(|(a, b, _)| (a.min(b), a.max(b)))
    }

    pub(super) fn select(&mut self, column: u16, row: u16, extend: bool) {
        if extend && !self.dragging {
            return;
        }
        let row = self.first
            + usize::from(
                row.saturating_sub(self.area.y)
                    .min(self.area.height.saturating_sub(1)),
            );
        let column = usize::from(column.saturating_sub(self.area.x));
        self.cache_rows();
        let rows = &self.cached().rows;
        let Some(row) = rows.get(row.min(rows.len().saturating_sub(1))) else {
            return;
        };
        let point = selection::point_at(row, column);
        match &mut self.selection {
            Some((_, end, moved)) if extend => {
                *end = point;
                *moved = true;
            }
            _ => {
                self.selection = Some((point, point, false));
                self.dragging = true;
            }
        }
    }

    pub(super) fn finish_selection(&mut self) -> Option<String> {
        self.dragging = false;
        self.selected_text()
    }

    fn selected_text(&self) -> Option<String> {
        let range = self.selected_range()?;
        selection::text(&self.cached().rows, range)
    }

    pub(super) fn clear(&mut self) {
        self.layout = None;
        self.clear_selection();
        self.cursor = 0;
        self.pastes.clear();
        self.text.clear();
    }

    pub(super) fn set(&mut self, text: String) -> bool {
        let text = Clean::new(&text).to_string();
        if text.len() > super::MAX_DRAFT_BYTES {
            return false;
        }
        self.cursor = text.chars().count();
        self.layout = None;
        self.clear_selection();
        self.text = text;
        self.pastes.clear();
        true
    }
}

impl DraftLayout {
    fn walk(&self, text: &str, mut visit: impl FnMut(usize, usize, usize, &str)) {
        let (mut index, mut byte, mut cells) = (0, 0, 0);
        for (row, source) in self.rows.iter().enumerate() {
            cells = source.indent;
            for (_, _, grapheme, _) in selection::graphemes(source) {
                visit(index, row, cells, grapheme);
                index += grapheme.chars().count();
                byte += grapheme.len();
                cells += super::text::width(grapheme);
            }
            if text[byte..].starts_with('\n') {
                visit(index, row, cells, "\n");
                index += 1;
                byte += 1;
            }
        }
        let row = self.rows.len().saturating_sub(1) + usize::from(cells >= self.width);
        visit(index, row, if cells >= self.width { 0 } else { cells }, "");
    }

    fn position(&self, text: &str, cursor: usize) -> (usize, usize) {
        let mut at = (0, 0);
        self.walk(text, |index, row, cells, grapheme| {
            if index <= cursor && cursor <= index + grapheme.chars().count() {
                let prefix: String = grapheme.chars().take(cursor - index).collect();
                at = (row, cells + text::width(&prefix));
            }
        });
        at
    }
}
