use super::{
    markdown::{RenderRow, selectable},
    search::{self, PromptKey, Search},
    selection::{self, Anchor, Point, Row},
    status::Status,
    transcript::{DetailSequence, Transcript},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{style::Modifier, text::Line};
use std::{collections::BTreeMap, num::NonZeroIsize, sync::Arc};

#[derive(PartialEq, Eq)]
struct IndexKey {
    width: u16,
    ui: crate::preferences::Ui,
    first: u64,
}

struct SearchIndex {
    key: IndexKey,
    next: u64,
    revision: u64,
}

#[derive(Clone, Copy)]
struct PendingScroll {
    origin: Option<Anchor>,
    delta: NonZeroIsize,
}

#[derive(Default)]
pub(super) struct Scrollback {
    heights: BTreeMap<u64, usize>,
    first: u64,
    next: u64,
    width: u16,
    ui: crate::preferences::Ui,
    tail: Arc<[RenderRow]>,
    anchor: Option<Anchor>,
    painted_origin: Option<Anchor>,
    pending: Option<PendingScroll>,
    height: usize,
    shown: Vec<Row>,
    selection: Option<(Point, Point, bool)>,
    pressed_detail: Option<DetailSequence>,
    hovered_row: Option<usize>,
    search: Search,
    index: Option<SearchIndex>,
    seek: bool,
}

impl Scrollback {
    pub(super) fn in_history(&self) -> bool {
        self.anchor.is_some() || (self.pending.is_some() && !self.shown.is_empty())
    }

    pub(super) fn search(&self) -> &Search {
        &self.search
    }

    pub(super) fn clear_search(&mut self) {
        self.search.clear();
        self.index = None;
        self.seek = false;
    }

    pub(super) fn search_key(&mut self, key: KeyEvent, composer_empty: bool) -> bool {
        if self.search.in_mode() {
            if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                return false;
            }
            match self.search.prompt_key(key) {
                PromptKey::Submitted => {
                    if self.search.active() {
                        self.pending = None;
                    } else {
                        self.index = None;
                        self.seek = false;
                    }
                }
                PromptKey::Consumed => {
                    if !self.search.active() {
                        self.index = None;
                    }
                }
                PromptKey::Unhandled => {
                    let reopen = (key.code == KeyCode::Char('f')
                        && key.modifiers == KeyModifiers::CONTROL)
                        || (key.code == KeyCode::Char('/') && key.modifiers.is_empty());
                    if reopen {
                        self.clear_search();
                        self.search.open();
                    } else if self.search.clear_key(key) {
                        self.index = None;
                        self.seek = false;
                    } else if let Some(delta) = match (key.code, key.modifiers) {
                        (KeyCode::Char('n') | KeyCode::Enter | KeyCode::Down, m)
                            if m.is_empty() =>
                        {
                            Some(1)
                        }
                        (KeyCode::Char('N'), m) if m.difference(KeyModifiers::SHIFT).is_empty() => {
                            Some(-1)
                        }
                        (KeyCode::Enter, m) if m == KeyModifiers::SHIFT => Some(-1),
                        (KeyCode::Up, m) if m.is_empty() => Some(-1),
                        _ => None,
                    } && self.search.step_by(delta)
                    {
                        self.pending = None;
                    }
                }
            }
            self.cancel_click();
            self.clear_hover();
            return true;
        }
        let open = (key.code == KeyCode::Char('f') && key.modifiers == KeyModifiers::CONTROL)
            || (key.code == KeyCode::Char('/')
                && key.modifiers.is_empty()
                && composer_empty
                && self.in_history());
        if open {
            self.clear_search();
            self.search.open();
            self.cancel_click();
            self.clear_hover();
            true
        } else {
            false
        }
    }

    pub(super) fn search_paste(&mut self, text: &str) -> Result<(), &'static str> {
        self.search.paste(text)
    }

    pub(super) fn prepare_search(&mut self, transcript: &Transcript, status: &mut Status) {
        if !self.search.active() {
            return;
        }
        let key = IndexKey {
            width: self.width,
            ui: self.ui,
            first: self.first,
        };
        let revision = transcript.revision();
        let reusable = self
            .index
            .as_ref()
            .filter(|index| self.search.indexed() && index.key == key && index.next <= self.next);
        if !reusable.is_some_and(|index| index.next == self.next && index.revision == revision) {
            self.index_search(transcript, reusable.map(|index| index.next));
            self.index = Some(SearchIndex {
                key,
                next: self.next,
                revision,
            });
        }
        self.seek |= self.search.take_seek(status).is_some();
    }

    fn index_search(&mut self, transcript: &Transcript, scan_from: Option<u64>) {
        let query = self.search.lower();
        let previous = self.search.target().filter(|_| !self.search.prompting());
        let keep = scan_from.map_or(0, |from| {
            self.search
                .matches()
                .partition_point(|hit| hit.anchor.0 < from)
        });
        let blocks = transcript.blocks();
        let start = scan_from.map_or(0, |from| blocks.partition_point(|entry| entry.id < from));
        let mut hits = Vec::new();
        for entry in blocks.iter().skip(start) {
            let rows = selectable(&entry.block.rows(self.width, self.ui), entry.id, self.width);
            hits.extend(selection::search(&rows, query));
        }
        hits.extend(selection::search(
            &selectable(transcript.tail(), self.next, self.width),
            query,
        ));
        let anchor = self.anchor;
        let reflow = self
            .index
            .as_ref()
            .is_some_and(|index| index.key.width != self.width || index.key.ui != self.ui);
        self.seek |=
            reflow && anchor.is_some() && previous.is_some_and(|hit| self.shows(hit.anchor));
        self.search.update_matches(keep, hits, |hits| {
            previous
                .and_then(|hit| {
                    hits.binary_search_by_key(&hit.start(), |hit| hit.start())
                        .ok()
                })
                .or_else(|| {
                    let top = anchor?;
                    hits.iter().position(|hit| hit.anchor >= top)
                })
                .or_else(|| hits.len().checked_sub(1))
        });
    }

    fn shows(&self, anchor: Anchor) -> bool {
        self.shown
            .binary_search_by_key(&anchor, |row| row.anchor)
            .is_ok()
    }

    fn cancel_layout_selection(&mut self, transcript: &Transcript) {
        let Some((a, b, _)) = self.selection else {
            return;
        };
        let (first, last) = (a.0.min(b.0), a.0.max(b.0));
        let blocks = transcript.blocks();
        let start = blocks.partition_point(|entry| entry.id < first);
        if blocks
            .iter()
            .skip(start)
            .take_while(|entry| entry.id <= last)
            .any(|entry| entry.block.layout_dependent())
        {
            self.selection = None;
        }
    }

    pub(super) fn resize(&mut self, width: u16, transcript: &Transcript) {
        if self.width != width {
            self.cancel_layout_selection(transcript);
            self.cancel_click();
            self.hovered_row = None;
            self.width = width;
            self.heights.clear();
        }
    }
    fn revalidate_selection(&mut self, width: u16, transcript: &Transcript) {
        if let Some((a, b, moved)) = self.selection
            && a.0.max(b.0) == self.next
            && (self.next != transcript.next() || !Arc::ptr_eq(&self.tail, transcript.tail()))
        {
            self.selection = self
                .rebase_tail(a, b, width, transcript)
                .map(|(a, b)| (a, b, moved));
        }
    }
    fn sync_tail(&mut self, width: u16, transcript: &Transcript) {
        self.revalidate_selection(width, transcript);
        self.tail = Arc::clone(transcript.tail());
        self.next = transcript.next();
    }
    pub(super) fn prepare(&mut self, width: u16, transcript: &Transcript) {
        self.sync_tail(width, transcript);
        self.resize(width, transcript);
        self.first = transcript.first();
        while self
            .heights
            .first_key_value()
            .is_some_and(|(&id, _)| id < self.first)
        {
            self.heights.pop_first();
        }
        if self.ui != transcript.ui() {
            self.cancel_layout_selection(transcript);
            self.heights.clear();
            self.ui = transcript.ui();
        }
    }
    fn rebase_tail(
        &self,
        a: Point,
        b: Point,
        width: u16,
        transcript: &Transcript,
    ) -> Option<(Point, Point)> {
        let live = |point: Point| (point.0 == self.next).then_some(point);
        let old = selectable(&self.tail, self.next, width);
        if self.next == transcript.next() {
            let new = selectable(transcript.tail(), self.next, width);
            return selection::unchanged(&old, &new, live(a), live(b)).then_some((a, b));
        }
        let blocks = transcript.blocks();
        let start = blocks.partition_point(|entry| entry.id < self.next);
        if blocks.get(start)?.id != self.next {
            return None;
        }
        let mut rows = Vec::new();
        let mut byte = 0;
        let mut destinations = Vec::new();
        let last = [live(a), live(b)]
            .into_iter()
            .flatten()
            .map(|(_, byte)| byte)
            .max()?;
        for entry in blocks.iter().skip(start) {
            let content = entry.block.rows(width, transcript.ui());
            let extent: usize = content.iter().map(RenderRow::source_extent).sum();
            destinations.push((byte..byte + extent, entry.id));
            rows.extend(content.iter().cloned());
            byte += extent;
            if byte > last {
                break;
            }
        }
        if byte <= last {
            destinations.push((byte..usize::MAX, transcript.next()));
            rows.extend(transcript.tail().iter().cloned());
        }
        let new = selectable(&rows, self.next, width);
        if !selection::unchanged(&old, &new, live(a), live(b)) {
            return None;
        }
        let translate = |point: Point| {
            if live(point).is_none() {
                return Some(point);
            }
            destinations
                .iter()
                .find(|(range, _)| range.contains(&point.1))
                .map(|(range, block)| (*block, point.1 - range.start))
        };
        Some((translate(a)?, translate(b)?))
    }
    pub(super) fn protected(&self) -> Option<(u64, u64)> {
        if !self.in_history() {
            return None;
        }
        Some((self.shown.first()?.anchor.0, self.shown.last()?.anchor.0))
    }
    pub(super) fn evicted(&mut self, ids: &[u64]) {
        if !ids.is_empty() {
            self.index = None;
            self.seek = false;
        }
        for id in ids {
            self.heights.remove(id);
        }
    }
    fn id(index: usize, transcript: &Transcript) -> u64 {
        transcript
            .blocks()
            .get(index)
            .map_or(transcript.next(), |entry| entry.id)
    }
    fn rows(&mut self, index: usize, transcript: &Transcript) -> Vec<Row> {
        let rows = if let Some(entry) = transcript.blocks().get(index) {
            selectable(
                &entry.block.rows(self.width, self.ui),
                Self::id(index, transcript),
                self.width,
            )
        } else {
            selectable(transcript.tail(), Self::id(index, transcript), self.width)
        };
        if index < transcript.blocks().len() {
            self.heights.insert(Self::id(index, transcript), rows.len());
        }
        rows
    }
    fn height(&mut self, index: usize, transcript: &Transcript, tail_height: usize) -> usize {
        if index == transcript.blocks().len() {
            return tail_height;
        }
        self.heights
            .get(&Self::id(index, transcript))
            .copied()
            .unwrap_or_else(|| self.rows(index, transcript).len())
    }
    fn move_rows(
        &mut self,
        mut block: usize,
        mut row: usize,
        delta: isize,
        transcript: &Transcript,
        tail_height: usize,
    ) -> (usize, usize) {
        let mut remaining = delta.unsigned_abs();
        if delta < 0 {
            loop {
                if remaining <= row {
                    return (block, row - remaining);
                }
                remaining -= row;
                if block == 0 {
                    return (0, 0);
                }
                block -= 1;
                row = self.height(block, transcript, tail_height);
            }
        } else {
            loop {
                let height = self.height(block, transcript, tail_height);
                if row + remaining < height {
                    return (block, row + remaining);
                }
                if block == transcript.blocks().len() {
                    return (block, height.saturating_sub(1));
                }
                remaining = (row + remaining).saturating_sub(height);
                block += 1;
                row = 0;
            }
        }
    }
    fn locate(&mut self, anchor: Anchor, transcript: &Transcript, tail: &[Row]) -> (usize, usize) {
        let block = transcript
            .blocks()
            .partition_point(|entry| entry.id < anchor.0);
        let rows = if block == transcript.blocks().len() {
            tail
        } else {
            &self.rows(block, transcript)
        };
        let row = rows
            .partition_point(|row| row.anchor <= anchor)
            .saturating_sub(1);
        (block, row)
    }

    pub(super) fn view(&mut self, height: usize, transcript: &Transcript) -> Vec<Line<'_>> {
        let resized = self.height != height;
        if resized {
            self.hovered_row = None;
        }
        self.height = height;
        if height == 0 {
            self.painted_origin = None;
            self.shown.clear();
            return Vec::new();
        }
        let page = isize::try_from(height).expect("terminal height fits in isize");
        let seek = std::mem::take(&mut self.seek);
        let target = self
            .search
            .target()
            .filter(|hit| {
                seek || (resized
                    && !self.search.prompting()
                    && self.anchor.is_some()
                    && self.shows(hit.anchor))
            })
            .map(|hit| hit.anchor);
        self.shown.clear();
        let last = transcript.blocks().len();
        let mut tail = self.rows(last, transcript);
        let tail_height = tail.len();
        let pending = self.pending.take();
        let origin = pending.and_then(|scroll| scroll.origin).or(self.anchor);
        let (mut block, mut row) = if let Some(target) = target {
            let (block, row) = self.locate(target, transcript, &tail);
            let context = (page - 1).min(2);
            let start = self.move_rows(block, row, -context, transcript, tail_height);
            let bottom = self.move_rows(last, tail_height, -page, transcript, tail_height);
            if start >= bottom {
                self.anchor = None;
                bottom
            } else {
                let rows = if start.0 == last {
                    &tail
                } else {
                    &self.rows(start.0, transcript)
                };
                self.anchor = rows.get(start.1).map(|row| row.anchor);
                start
            }
        } else if let Some(anchor) = origin {
            let start = self.locate(anchor, transcript, &tail);
            if start.0 == last && tail_height == 0 {
                self.anchor = None;
                self.move_rows(last, 0, -page, transcript, tail_height)
            } else {
                start
            }
        } else {
            self.move_rows(last, tail_height, -page, transcript, tail_height)
        };
        if let Some(scroll) = pending {
            (block, row) = self.move_rows(block, row, scroll.delta.get(), transcript, tail_height);
            let rows = if block == last {
                &tail
            } else {
                &self.rows(block, transcript)
            };
            self.anchor = rows.get(row).map(|row| row.anchor);
        }
        if self.anchor.is_some() && pending.is_some_and(|scroll| scroll.delta.get() > 0) {
            let (end_block, end_row) = self.move_rows(block, row, page, transcript, tail_height);
            if end_block == last && end_row >= tail_height.saturating_sub(1) {
                self.anchor = None;
                let end = tail_height;
                (block, row) = self.move_rows(last, end, -page, transcript, tail_height);
            }
        }
        let capacity = height;
        while block <= transcript.blocks().len() && self.shown.len() < capacity {
            let rows = if block == last {
                std::mem::take(&mut tail)
            } else {
                self.rows(block, transcript)
            };
            self.shown
                .extend(rows.into_iter().skip(row).take(capacity - self.shown.len()));
            block += 1;
            row = 0;
        }
        self.painted_origin = self.shown.first().map(|row| row.anchor);
        if self.first > 0
            && self.shown.first().is_some_and(|row| {
                row.anchor == (self.first, 0, 0) && self.search.marks(row).is_empty()
            })
        {
            const OMITTED: &str = "… older display rows omitted";
            self.shown[0] = Row::chrome(self.shown[0].anchor, Line::from(OMITTED));
        }
        let selection = self.selected_range();
        let hovered = selection
            .is_none()
            .then_some(self.hovered_row)
            .flatten()
            .and_then(|row| self.shown.get(row))
            .and_then(Row::detail);
        let mut lines: Vec<_> = self
            .shown
            .iter()
            .map(|row| {
                let mut line = search::paint(row, selection, Some(&self.search));
                if hovered.is_some() && row.detail() == hovered {
                    for span in &mut line.spans {
                        if !span.content.trim().is_empty() {
                            span.style = span.style.add_modifier(Modifier::UNDERLINED);
                        }
                    }
                }
                line
            })
            .collect();
        for line in &mut lines {
            let pad = usize::from(self.width).saturating_sub(line.width());
            if line.style.bg.is_some() && pad > 0 {
                line.spans.push(" ".repeat(pad).into());
            }
        }
        lines
    }
    pub(super) fn scroll(&mut self, down: bool) {
        self.scroll_by(down, self.height.saturating_sub(1).max(1));
    }
    pub(super) fn scroll_line(&mut self, down: bool) {
        self.scroll_by(down, 1);
    }
    fn scroll_by(&mut self, down: bool, rows: usize) {
        self.cancel_click();
        self.hovered_row = None;
        if down
            && self.anchor.is_none()
            && self.pending.is_none()
            && !self.search.has_pending_seek()
            && !self.seek
        {
            return;
        }
        let rows = isize::try_from(rows).expect("terminal height fits in isize");
        let delta = if down { rows } else { -rows };
        let (origin, previous) = self
            .pending
            .map_or((self.painted_origin.or(self.anchor), 0), |scroll| {
                (scroll.origin, scroll.delta.get())
            });
        self.pending = NonZeroIsize::new(previous.saturating_add(delta))
            .map(|delta| PendingScroll { origin, delta });
    }
    pub(super) fn follow(&mut self) {
        self.cancel_click();
        self.hovered_row = None;
        self.anchor = None;
        self.pending = None;
        self.painted_origin = None;
        self.search.cancel_seek();
        self.seek = false;
    }
    pub(super) fn hover(&mut self, row: Option<usize>) {
        self.hovered_row = if self.selected_range().is_some() {
            None
        } else {
            row
        };
    }
    pub(super) fn clear_hover(&mut self) {
        self.hovered_row = None;
    }
    pub(super) fn cancel_click(&mut self) {
        self.pressed_detail = None;
    }
    pub(super) fn press(&mut self, row: usize, column: usize, clickable: bool) {
        self.pressed_detail = clickable
            .then(|| self.shown.get(row).and_then(Row::detail))
            .flatten();
        self.select(row, column, false);
    }
    pub(super) fn release(&mut self, row: Option<usize>) -> Option<DetailSequence> {
        let pressed = self.pressed_detail.take()?;
        if self.selected_range().is_some() {
            return None;
        }
        let released = row
            .and_then(|row| self.shown.get(row))
            .and_then(Row::detail);
        if released == Some(pressed) {
            self.clear_selection();
            self.hovered_row = None;
            Some(pressed)
        } else {
            None
        }
    }
    pub(super) fn select(&mut self, row: usize, column: usize, extend: bool) {
        if extend {
            self.cancel_click();
            self.hovered_row = None;
        }
        if extend && self.selection.is_none() {
            return;
        }
        let Some(row) = self.shown.get(row.min(self.shown.len().saturating_sub(1))) else {
            return;
        };
        let point = selection::point_at(row, column);
        match &mut self.selection {
            Some((_, end, dragged)) if extend => {
                *end = point;
                *dragged = true;
            }
            _ => self.selection = Some((point, point, false)),
        }
    }
    pub(super) fn select_beyond(&mut self, up: bool, column: usize) {
        self.select(
            if up {
                0
            } else {
                self.shown.len().saturating_sub(1)
            },
            column,
            true,
        );
        self.scroll_line(!up);
    }
    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
    }
    fn selected_range(&self) -> Option<(Point, Point)> {
        self.selection
            .filter(|(_, _, dragged)| *dragged)
            .map(|(a, b, _)| (a.min(b), a.max(b)))
    }
    pub(super) fn selected_text(&mut self, transcript: &mut Transcript) -> Option<String> {
        if self.ui != transcript.ui() {
            self.cancel_layout_selection(transcript);
            self.heights.clear();
            self.ui = transcript.ui();
        }
        if self
            .selection
            .is_some_and(|(a, b, _)| a.0.max(b.0) == self.next)
        {
            transcript.sync_tail(self.width);
        }
        self.revalidate_selection(self.width, transcript);
        let range = self.selected_range()?;
        let start = transcript
            .blocks()
            .partition_point(|entry| entry.id < range.0.0);
        let end = transcript
            .blocks()
            .partition_point(|entry| entry.id < range.1.0);
        selection::text(
            (start..=end).flat_map(|index| self.rows(index, transcript)),
            range,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locate(
        scrollback: &mut Scrollback,
        transcript: &Transcript,
        needle: &str,
    ) -> (usize, usize) {
        scrollback
            .view(32, transcript)
            .iter()
            .enumerate()
            .find_map(|(row, projected)| {
                let line = projected.to_string();
                line.find(needle)
                    .map(|byte| (row, super::super::text::width(&line[..byte])))
            })
            .expect(needle)
    }

    fn find(scrollback: &mut Scrollback, query: &str) {
        assert!(scrollback.search_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            true
        ));
        scrollback.search_paste(query).unwrap();
        assert!(scrollback.search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), true));
    }

    fn collect_marks(lines: &[Line<'_>]) -> String {
        use ratatui::style::Modifier;
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| {
                span.style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::REVERSED)
            })
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn search_paints_the_visible_end_of_a_match_across_a_prose_wrap() {
        let mut transcript = Transcript::default();
        let message = transcript.user("alpha beta gamma", &[], false);
        let later = transcript.user("later", &[], false);
        transcript.append([message, later], None);
        let mut scrollback = Scrollback::default();
        let mut status = Status::default();
        scrollback.prepare(10, &transcript);
        find(&mut scrollback, "alpha beta");
        scrollback.prepare_search(&transcript, &mut status);
        let lines = scrollback.view(2, &transcript);
        assert_eq!(collect_marks(&lines[..1]).trim(), "alpha");
        assert_eq!(collect_marks(&lines[1..]).trim(), "beta");
        scrollback.scroll_line(true);
        assert_eq!(collect_marks(&scrollback.view(2, &transcript)), "beta");
    }

    #[test]
    fn search_keeps_a_wrapped_table_cell_in_view_after_resize() {
        use crate::tui::session::UiEvent;
        let mut transcript = Transcript::default();
        let blocks = transcript.apply(
            UiEvent::Text("| Name | Data |\n| --- | ---: |\n| x | abcdefg   hij klm |".into()),
            23,
        );
        transcript.append(blocks, None);
        let blocks = transcript.end(None, 23);
        transcript.append(blocks, None);
        for _ in 0..40 {
            let block = transcript.user("filler", &[], false);
            transcript.append([block], None);
        }
        let mut scrollback = Scrollback::default();
        let mut status = Status::default();
        scrollback.prepare(23, &transcript);
        find(&mut scrollback, "g   hij");
        scrollback.prepare_search(&transcript, &mut status);
        assert_eq!(
            scrollback.search.step_hint().as_deref(),
            Some("n/N find [1/1]")
        );
        let lines = scrollback.view(12, &transcript);
        assert_eq!(collect_marks(&lines).replace(' ', ""), "ghij");
        scrollback.prepare(11, &transcript);
        scrollback.prepare_search(&transcript, &mut status);
        let lines = scrollback.view(7, &transcript);
        assert!(lines.iter().any(|line| line.to_string().contains("Data:")));
        assert_eq!(collect_marks(&lines).replace(' ', ""), "ghij");
        for chrome in ["─", "╌", "Data:", "▌"] {
            find(&mut scrollback, chrome);
            scrollback.prepare_search(&transcript, &mut status);
            assert_eq!(status.text(), Some("No matches"));
        }
    }

    #[test]
    fn search_updates_for_live_tail_and_eviction() {
        use crate::tui::session::UiEvent;
        let mut transcript = Transcript::default();
        let block = transcript.user("alpha needle", &[], false);
        transcript.append([block], None);
        let mut scrollback = Scrollback::default();
        let mut status = Status::default();
        let width = 20;
        let paint =
            |scrollback: &mut Scrollback, transcript: &mut Transcript, status: &mut Status| {
                transcript.prepare_tail(width, 0);
                scrollback.prepare(width, transcript);
                scrollback.prepare_search(transcript, status);
                collect_marks(&scrollback.view(12, transcript))
            };
        find(&mut scrollback, "needle");
        assert_eq!(
            paint(&mut scrollback, &mut transcript, &mut status),
            "needle"
        );
        let blocks = transcript.apply(UiEvent::Text("live nee".into()), width);
        transcript.append(blocks, None);
        assert_eq!(
            paint(&mut scrollback, &mut transcript, &mut status),
            "needle"
        );
        let blocks = transcript.apply(UiEvent::Text("dle".into()), width);
        transcript.append(blocks, None);
        assert_eq!(
            paint(&mut scrollback, &mut transcript, &mut status),
            "needleneedle"
        );
        assert_eq!(
            scrollback.search.step_hint().as_deref(),
            Some("n/N find [1/2]")
        );
        let blocks = transcript.end(None, width);
        transcript.append(blocks, None);
        assert_eq!(
            paint(&mut scrollback, &mut transcript, &mut status),
            "needleneedle"
        );
        let block = transcript.user("new needle", &[], false);
        transcript.append([block], None);
        assert_eq!(
            paint(&mut scrollback, &mut transcript, &mut status),
            "needleneedleneedle"
        );
        assert_eq!(
            scrollback.search.step_hint().as_deref(),
            Some("n/N find [1/3]")
        );
        let large = "filler\n".repeat(500);
        assert!((0..80).any(|_| {
            let block = transcript.user(&large, &[], false);
            let removed = transcript.append([block], None);
            scrollback.evicted(&removed);
            transcript.first() >= 3
        }));
        scrollback.follow();
        assert_eq!(paint(&mut scrollback, &mut transcript, &mut status), "");
        assert!(scrollback.search.step_hint().is_none());
        assert!(scrollback.search_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), true));
        assert!(scrollback.search.in_mode());
        assert!(scrollback.search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true));
        assert!(!scrollback.search.in_mode());
    }

    #[test]
    fn tail_drag_survives_deltas_and_commit_but_not_a_changed_target() {
        use crate::tui::session::UiEvent;

        let mut transcript = Transcript::default();
        let mut scrollback = Scrollback::default();
        let push = |transcript: &mut Transcript, scrollback: &mut Scrollback, event| {
            let blocks = transcript.apply(event, 32);
            transcript.append(blocks, None);
            let blocks = transcript.flush_frame();
            transcript.append(blocks, None);
            transcript.prepare_tail(32, 0);
            scrollback.prepare(32, transcript);
        };
        push(
            &mut transcript,
            &mut scrollback,
            UiEvent::Text("First paragraph\n\nUnchanged sentence".into()),
        );
        let (row, column) = locate(&mut scrollback, &transcript, "Unchanged sentence");
        scrollback.select(row, column, false);
        push(
            &mut transcript,
            &mut scrollback,
            UiEvent::Text(" with a suffix".into()),
        );
        let (row, column) = locate(&mut scrollback, &transcript, "Unchanged sentence");
        scrollback.select(row, column + "Unchanged sentence".len() - 1, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some("Unchanged sentence")
        );
        push(
            &mut transcript,
            &mut scrollback,
            UiEvent::Text("\n\n**pending text".into()),
        );
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some("Unchanged sentence")
        );
        let (row, column) = locate(&mut scrollback, &transcript, "**pending text");
        scrollback.select(row, column, false);
        scrollback.select(row, column + 4, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some("**pen")
        );
        push(&mut transcript, &mut scrollback, UiEvent::Text("**".into()));
        assert_eq!(scrollback.selected_text(&mut transcript), None);
    }

    #[test]
    fn reasoning_wrapping_keeps_the_first_line_and_uses_one_marker_per_heading() {
        use crate::tui::session::UiEvent;

        let summary = "a complete first reasoning line";
        let heading = "a long final heading";
        let mut transcript = Transcript::default();
        let event = UiEvent::Reasoning(format!("{summary}\nignored prose\n\n# {heading}"));
        let blocks = transcript.apply(event, 12);
        transcript.append(blocks, None);
        let blocks = transcript.end(None, 12);
        transcript.append(blocks, None);
        let mut scrollback = Scrollback::default();
        scrollback.prepare(12, &transcript);
        let lines = scrollback.view(32, &transcript);
        let painted = lines.iter().map(ToString::to_string).collect::<String>();
        assert_eq!(painted.matches('◆').count(), 2);
        assert!(!painted.contains("ignored"));
        let end = lines.len() - 1;
        scrollback.select(0, 0, false);
        scrollback.select(end, 11, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some(format!("◆ {summary}\n◆ {heading}").as_str())
        );
    }

    #[test]
    fn copying_across_stored_blocks_preserves_prose_table_and_blank_lines() {
        let source = "Before\n\n| Header |\n| --- |\n| cell |\n\nAfter";
        let mut transcript = Transcript::default();
        let blocks = transcript.apply(crate::tui::session::UiEvent::Text(source.into()), 24);
        transcript.append(blocks, None);
        let blocks = transcript.end(None, 24);
        transcript.append(blocks, None);
        let mut scrollback = Scrollback::default();
        scrollback.prepare(24, &transcript);
        let (row, column) = locate(&mut scrollback, &transcript, "Before");
        scrollback.select(row, column, false);
        let (row, column) = locate(&mut scrollback, &transcript, "After");
        scrollback.select(row, column + 4, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some(source)
        );
    }

    #[test]
    fn copied_table_cell_survives_scrollback_resize() {
        let mut transcript = Transcript::default();
        let blocks = transcript.apply(
            crate::tui::session::UiEvent::Text(
                "| Name | Data |\n| --- | ---: |\n| x | abcdefg   hij klm |".into(),
            ),
            23,
        );
        transcript.append(blocks, None);
        let blocks = transcript.end(None, 23);
        transcript.append(blocks, None);
        let mut scrollback = Scrollback::default();
        scrollback.prepare(23, &transcript);
        let lines: Vec<_> = scrollback
            .view(32, &transcript)
            .iter()
            .map(ToString::to_string)
            .collect();
        let locate = |part: &str| {
            lines
                .iter()
                .enumerate()
                .find_map(|(row, line)| {
                    line.find(part)
                        .map(|byte| (row, super::super::text::width(&line[..byte])))
                })
                .expect(part)
        };
        let (row, col) = locate("abcdefg");
        scrollback.select(row, col, false);
        let (row, col) = locate("klm");
        scrollback.select(row, col + 2, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some("abcdefg   hij klm")
        );
        scrollback.prepare(11, &transcript);
        assert!(
            scrollback
                .view(32, &transcript)
                .iter()
                .any(|line| line.to_string().contains("Data: "))
        );
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some("abcdefg   hij klm")
        );
    }

    #[test]
    fn mouse_up_revalidates_live_tail_selection_before_copying() {
        use crate::tui::session::UiEvent;

        // Uncommitted delta changes live tail formatting before prepare_tail runs.
        let mut transcript = Transcript::default();
        let mut scrollback = Scrollback::default();
        let blocks = transcript.apply(UiEvent::Text("**pending".into()), 40);
        transcript.append(blocks, None);
        transcript.prepare_tail(40, 0);
        scrollback.prepare(40, &transcript);
        let (row, col) = locate(&mut scrollback, &transcript, "**pen");
        scrollback.select(row, col, false);
        scrollback.select(row, col + 4, true);
        let blocks = transcript.apply(UiEvent::Text("**".into()), 40);
        transcript.append(blocks, None);
        assert_eq!(scrollback.selected_text(&mut transcript), None);
        // Repeating the drag at the same unpainted coordinates still rejects the stale tail.
        scrollback.select(row, col, false);
        scrollback.select(row, col + 4, true);
        assert_eq!(scrollback.selected_text(&mut transcript), None);

        // Resizing a completed tool cell cancels its width-dependent selection instead of copying padding.
        let mut transcript = Transcript::default();
        transcript.set_ui(crate::preferences::Ui::Cells);
        let blocks = transcript.apply(
            UiEvent::ToolStarted {
                id: "t1".into(),
                name: "python".into(),
                arguments: serde_json::json!({"code": "print('ok')"}).into(),
                live: None,
            },
            40,
        );
        transcript.append(blocks, None);
        let blocks = transcript.apply(
            UiEvent::ToolFinished {
                id: "t1".into(),
                output: "ok".into(),
                status: crate::tool::Status::Succeeded,
                elapsed: None,
                jobs: Vec::new(),
                diffs: Vec::new(),
                activities: Default::default(),
            },
            40,
        );
        transcript.append(blocks, None);
        let blocks = transcript.end(None, 40);
        transcript.append(blocks, None);
        let mut scrollback = Scrollback::default();
        scrollback.prepare(40, &transcript);
        let (row, col) = locate(&mut scrollback, &transcript, "1 line");
        scrollback.select(row, col, false);
        scrollback.select(row, col + 5, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some("1 line")
        );
        scrollback.resize(80, &transcript);
        assert_eq!(scrollback.selected_text(&mut transcript), None);
    }

    #[test]
    fn wrapped_user_message_does_not_copy_gutter_or_indent() {
        let source = "first alpha beta gamma\n  next delta epsilon";
        let mut transcript = Transcript::default();
        let block = transcript.user(source, &[], false);
        transcript.append([block], None);
        let mut scrollback = Scrollback::default();
        scrollback.prepare(14, &transcript);
        let lines = scrollback.view(32, &transcript);
        let start = lines
            .iter()
            .position(|line| line.to_string().contains("first"))
            .unwrap();
        let end = lines.len() - 1;
        assert!(end > start + 1);
        assert!(
            lines[start..]
                .iter()
                .all(|line| line.to_string().starts_with("▌ "))
        );
        scrollback.select(start, 0, false);
        scrollback.select(end, 13, true);
        assert_eq!(
            scrollback.selected_text(&mut transcript).as_deref(),
            Some(source)
        );
    }
}
