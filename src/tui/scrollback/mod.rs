use super::{
    markdown::RenderRow,
    selection::{self, Point, Row},
    status::Status,
    transcript::Transcript,
};
use ratatui::text::Line;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Scrollback {
    heights: BTreeMap<u64, usize>,
    first: u64,
    width: u16,
    ui: crate::preferences::Ui,
    revision: u64,
    anchor: Option<Point>,
    pending: isize,
    height: usize,
    shown: Vec<Row>,
    selection: Option<(Point, Point, bool)>,
}

fn wrapped(source: &[RenderRow], block: u64, width: u16) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut byte = 0;
    for logical in source {
        for line in logical.project(width) {
            let len = line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            if line
                .spans
                .iter()
                .map(|span| super::text::width(&span.content))
                .sum::<usize>()
                <= usize::from(width.max(1))
                && !line.spans.iter().any(|span| span.content.contains('\n'))
            {
                rows.push(Row {
                    block,
                    byte,
                    len,
                    indent: 0,
                    line,
                });
                byte += len + 1;
                continue;
            }
            let wrapped = selection::wrap_line(line, block, byte, usize::from(width));
            byte = wrapped.last().map_or(byte, |row| row.byte + row.len) + 1;
            rows.extend(wrapped);
        }
    }
    rows
}

impl Scrollback {
    pub(super) fn in_history(&self) -> bool {
        self.anchor.is_some()
    }

    pub(super) fn resize(&mut self, width: u16, transcript: &Transcript, status: &mut Status) {
        if self.width != width {
            if let Some((a, b, _)) = self.selection
                && transcript.blocks().iter().any(|entry| {
                    (a.0.min(b.0)..=a.0.max(b.0)).contains(&entry.id)
                        && entry.block.rows.iter().any(RenderRow::is_table)
                })
            {
                self.selection = None;
                status.set(
                    super::status::Kind::Info,
                    super::text::Clean::literal("Table selection cleared"),
                    Some(std::time::Duration::from_secs(3)),
                );
            }
            if let Some((id, byte)) = &mut self.anchor
                && transcript
                    .blocks()
                    .iter()
                    .find(|entry| entry.id == *id)
                    .is_some_and(|entry| entry.block.rows.iter().any(RenderRow::is_table))
            {
                *byte = 0;
            }
            self.width = width;
            self.heights.clear();
        }
    }
    pub(super) fn prepare(&mut self, width: u16, transcript: &Transcript, status: &mut Status) {
        if let Some((a, b, _)) = self.selection {
            let resized = self.width != width;
            let changed = self.revision != transcript.revision();
            let table_selected = transcript.blocks().iter().any(|entry| {
                (a.0.min(b.0)..=a.0.max(b.0)).contains(&entry.id)
                    && entry.block.rows.iter().any(RenderRow::is_table)
            }) || transcript.tail().iter().any(RenderRow::is_table)
                && a.0.max(b.0) >= transcript.next();
            if table_selected && (resized || changed) {
                self.selection = None;
                status.set(
                    super::status::Kind::Info,
                    super::text::Clean::literal("Table selection cleared"),
                    Some(std::time::Duration::from_secs(3)),
                );
            }
        }
        self.resize(width, transcript, status);
        self.first = transcript.first();
        while self
            .heights
            .first_key_value()
            .is_some_and(|(&id, _)| id < self.first)
        {
            self.heights.pop_first();
        }
        if self.ui != transcript.ui() {
            self.heights.clear();
            self.ui = transcript.ui();
        }
        self.revision = transcript.revision();
    }
    pub(super) fn protected(&self) -> Option<(u64, u64)> {
        self.anchor?;
        Some((self.shown.first()?.block, self.shown.last()?.block))
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
            wrapped(
                &entry.block.rows(self.width, self.ui),
                Self::id(index, transcript),
                self.width,
            )
        } else {
            wrapped(transcript.tail(), Self::id(index, transcript), self.width)
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
        let (mut block, mut row) = if let Some((id, byte)) = self.anchor {
            let block = transcript.blocks().partition_point(|entry| entry.id < id);
            let rows = if block == last {
                &tail
            } else {
                &self.rows(block, transcript)
            };
            let row = rows
                .partition_point(|row| row.byte <= byte)
                .saturating_sub(1);
            (block, row)
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
            self.anchor = rows.get(row).map(|row| (row.block, row.byte));
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
        let selection = self.selected_range();
        let mut lines: Vec<_> = self
            .shown
            .iter()
            .map(|row| match selection {
                Some(range) => selection::highlight(row, range),
                None => selection::borrowed(&row.line),
            })
            .collect();
        for line in &mut lines {
            let pad = usize::from(self.width).saturating_sub(line.width());
            if line.style.bg.is_some() && pad > 0 {
                line.spans.push(" ".repeat(pad).into());
            }
        }
        if self.first > 0
            && self
                .shown
                .first()
                .is_some_and(|row| row.block == self.first && row.byte == 0)
            && !lines.is_empty()
        {
            lines[0] = Line::from("… older display rows omitted");
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
            self.anchor = Some((row.block, row.byte));
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
    pub(super) fn selected_text(&mut self, transcript: &Transcript) -> Option<String> {
        let range = self.selected_range()?;
        let start = transcript
            .blocks()
            .partition_point(|entry| entry.id < range.0.0);
        let end = transcript
            .blocks()
            .partition_point(|entry| entry.id < range.1.0);
        let mut rows = Vec::new();
        for index in start..=end {
            rows.extend(self.rows(index, transcript));
        }
        selection::text(&rows, range)
    }
}
