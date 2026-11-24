use super::{
    markdown::{RenderRow, selectable},
    selection::{self, Anchor, Point, Row},
    transcript::Transcript,
};
use ratatui::text::Line;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Default)]
pub(super) struct Scrollback {
    heights: BTreeMap<u64, usize>,
    first: u64,
    next: u64,
    width: u16,
    ui: crate::preferences::Ui,
    tail: Arc<[RenderRow]>,
    anchor: Option<Anchor>,
    pending: isize,
    height: usize,
    shown: Vec<Row>,
    selection: Option<(Point, Point, bool)>,
}

impl Scrollback {
    pub(super) fn in_history(&self) -> bool {
        self.anchor.is_some()
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
            self.width = width;
            self.heights.clear();
        }
    }
    fn revalidate_selection(&mut self, width: u16, transcript: &Transcript) {
        if let Some((a, b, moved)) = self.selection
            && a.0.max(b.0) == self.next
            && (self.next != transcript.next() || self.tail.as_ref() != transcript.tail().as_ref())
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
        self.anchor?;
        Some((self.shown.first()?.anchor.0, self.shown.last()?.anchor.0))
    }
    pub(super) fn evicted(&mut self, ids: &[u64]) {
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
    pub(super) fn view(&mut self, height: usize, transcript: &Transcript) -> Vec<Line<'_>> {
        self.height = height;
        self.shown.clear();
        if height == 0 {
            return Vec::new();
        }
        let last = transcript.blocks().len();
        let mut tail = self.rows(last, transcript);
        let tail_height = tail.len();
        let (mut block, mut row) = if let Some(anchor @ (id, ..)) = self.anchor {
            let block = transcript.blocks().partition_point(|entry| entry.id < id);
            if block == last && tail_height == 0 {
                self.anchor = None;
                self.move_rows(last, 0, -(height as isize), transcript, tail_height)
            } else {
                let rows = if block == last {
                    &tail
                } else {
                    &self.rows(block, transcript)
                };
                let row = rows
                    .partition_point(|row| row.anchor <= anchor)
                    .saturating_sub(1);
                (block, row)
            }
        } else {
            let end = tail_height;
            self.move_rows(last, end, -(height as isize), transcript, tail_height)
        };
        let pending = std::mem::take(&mut self.pending);
        if pending != 0 {
            (block, row) = self.move_rows(block, row, pending, transcript, tail_height);
            let rows = if block == last {
                &tail
            } else {
                &self.rows(block, transcript)
            };
            self.anchor = rows.get(row).map(|row| row.anchor);
        }
        if self.anchor.is_some() && pending > 0 {
            let (end_block, end_row) =
                self.move_rows(block, row, height as isize, transcript, tail_height);
            if end_block == last && end_row >= tail_height.saturating_sub(1) {
                self.anchor = None;
                let end = tail_height;
                (block, row) =
                    self.move_rows(last, end, -(height as isize), transcript, tail_height);
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
        if self.first > 0
            && self
                .shown
                .first()
                .is_some_and(|row| row.anchor == (self.first, 0, 0))
        {
            const OMITTED: &str = "… older display rows omitted";
            self.shown[0] = Row::chrome(self.shown[0].anchor, Line::from(OMITTED));
        }
        let selection = self.selected_range();
        let mut lines: Vec<_> = self
            .shown
            .iter()
            .map(|row| match selection {
                Some(range) => selection::highlight(row, range),
                None => selection::borrowed(row.line()),
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
        if self.anchor.is_none()
            && let Some(row) = self.shown.first()
        {
            self.anchor = Some(row.anchor);
        }
        self.pending += if down {
            rows as isize
        } else {
            -(rows as isize)
        };
    }
    pub(super) fn follow(&mut self) {
        self.anchor = None;
        self.pending = 0;
    }
    pub(super) fn select(&mut self, row: usize, column: usize, extend: bool) {
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
