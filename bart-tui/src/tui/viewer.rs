use super::{
    brand,
    selection::{self, Anchor, Point, Row, point_at},
    status::{Kind, Status},
    text::{self, Clean, Item},
    transcript::{Detail, DetailSequence, Highlight},
    widgets::confirm::FrameStamp,
};
use crate::{
    detail_excerpt::{self, CODE_BYTES as MAX_CODE_BYTES, OUTPUT_BYTES as MAX_OUTPUT_BYTES},
    tool::{Live, LiveState, Snapshot},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::{borrow::Cow, ops::Range};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selection {
    Detail(DetailSequence),
    Job(u64),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Code,
    Output,
}

pub(super) enum Action {
    Close,
    Copy { text: String, what: &'static str },
}
#[derive(Clone, Copy)]
struct Drag {
    focus: Focus,
    start: Point,
    end: Point,
    moved: bool,
}
impl Drag {
    fn range(self) -> (Point, Point) {
        (self.start.min(self.end), self.start.max(self.end))
    }
}
const MAX_SEARCH_BYTES: usize = 1024;
#[derive(Default)]
struct Search {
    prompt: bool,
    query: String,
    selected: usize,
    seek: bool,
    matches: Vec<usize>,
    marks: Vec<Vec<Range<usize>>>,
    output_source: Option<u64>,
    output_key: Option<(Selection, u16, u64)>,
}
struct Pane {
    scroll: usize,
    anchor: Option<Anchor>,
    lines: Vec<Row>,
    area: Rect,
    label: Rect,
}
impl Pane {
    fn new(follow: bool) -> Self {
        Self {
            scroll: 0,
            anchor: (!follow).then_some((0, 0, 0)),
            lines: Vec::new(),
            area: Rect::default(),
            label: Rect::default(),
        }
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        let rows = usize::from(self.area.height);
        let page = rows.saturating_sub(1).max(1);
        let half = (rows / 2).max(1);
        let control = key.modifiers == KeyModifiers::CONTROL;
        self.scroll = match key.code {
            KeyCode::Char('d') if control => self.scroll.saturating_add(half),
            KeyCode::Char('u') if control => self.scroll.saturating_sub(half),
            _ if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => return false,
            KeyCode::Down | KeyCode::Char('j') => self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll.saturating_add(page),
            KeyCode::PageUp => self.scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.anchor = None;
                return true;
            }
            _ => return false,
        };
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(rows));
        self.anchor = Some(
            self.lines
                .get(self.scroll)
                .map_or((0, 0, 0), |row| row.anchor),
        );
        true
    }
    fn place(&mut self) {
        let bottom = self
            .lines
            .len()
            .saturating_sub(usize::from(self.area.height));
        self.scroll = self.anchor.map_or(bottom, |anchor| {
            self.lines
                .partition_point(|row| row.anchor <= anchor)
                .saturating_sub(1)
                .min(bottom)
        });
    }
    fn draw(
        &self,
        frame: &mut Frame,
        heading: &str,
        focused: bool,
        range: Option<(Point, Point)>,
        marks: &[Vec<Range<usize>>],
    ) {
        for (index, row) in self
            .lines
            .iter()
            .skip(self.scroll)
            .take(usize::from(self.area.height))
            .enumerate()
        {
            let line = if let Some(range) = range {
                selection::highlight(row, range)
            } else {
                selection::borrowed(row.line())
            };
            frame.render_widget(
                match marks.get(self.scroll + index) {
                    Some(ranges) if !ranges.is_empty() => highlighted(&line, ranges),
                    _ => selection::borrowed(&line),
                },
                Rect::new(self.area.x, self.area.y + index as u16, self.area.width, 1),
            );
        }
        frame.render_widget(pane_heading(heading, self, focused), self.label);
    }
}
pub(super) struct Viewer {
    selected: Selection,
    code: Pane,
    output: Pane,
    focus: Focus,
    search: Search,
    drag: Option<Drag>,
    size: Size,
    heading: Line<'static>,
    output_heading: String,
    compact_title: String,
    has_code: bool,
    empty: bool,
    waiting: bool,
    painted: Option<FrameStamp>,
    code_key: Option<(Selection, u16, usize, Option<String>)>,
}
struct Content<'a> {
    title: String,
    review: Option<&'a str>,
    code: Cow<'a, str>,
    code_complete: bool,
    output: Cow<'a, str>,
    output_complete: bool,
    ui_notice_truncated: bool,
    highlights: Cow<'a, [Highlight]>,
    state: Option<LiveState>,
    markdown: bool,
}

impl Viewer {
    pub(super) fn open_detail(details: &[Detail], detail: DetailSequence) -> Option<Self> {
        details
            .iter()
            .any(|entry| entry.sequence == detail)
            .then(|| Self::with_selection(Selection::Detail(detail), details))
    }

    fn with_selection(selected: Selection, details: &[Detail]) -> Self {
        Self {
            selected,
            code: Pane::new(false),
            output: Pane::new(!is_ui_notice(selected, details)),
            focus: Focus::Output,
            search: Search::default(),

            drag: None,
            size: Size::default(),
            heading: Line::default(),
            output_heading: String::new(),
            compact_title: String::new(),
            has_code: false,
            empty: true,
            waiting: false,
            painted: None,
            code_key: None,
        }
    }

    pub(super) fn new(details: &[Detail], live: &Live, latest: bool) -> Self {
        let active = details
            .iter()
            .filter_map(|detail| {
                let id = detail.live_run_id?;
                (live.snapshot(id)?.summary.state == LiveState::Running)
                    .then_some((id, Selection::Detail(detail.sequence)))
            })
            .chain(
                live.jobs()
                    .into_iter()
                    .filter(|job| job.state == LiveState::Running)
                    .map(|job| (job.id, Selection::Job(job.id))),
            )
            .max_by_key(|(id, _)| *id);
        let preferred = details
            .iter()
            .rev()
            .find(|detail| latest || detail.compaction_running())
            .map(|detail| Selection::Detail(detail.sequence));
        let selected = preferred.unwrap_or_else(|| {
            active.map_or_else(
                || {
                    *selections(details, live)
                        .last()
                        .unwrap_or(&Selection::Detail(DetailSequence::default()))
                },
                |(_, selection)| selection,
            )
        });
        Self::with_selection(selected, details)
    }

    fn select(&mut self, selection: Selection, details: &[Detail]) {
        self.drag = None;
        self.selected = selection;
        self.code = Pane::new(false);
        self.code_key = None;
        self.output = Pane::new(!is_ui_notice(selection, details));
        self.search = Search::default();
        self.focus = Focus::Output;
    }

    pub(super) fn painted(&mut self, stamp: Option<FrameStamp>) {
        self.painted = stamp;
    }
    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        if !self.search.prompt {
            return Err("paste refused: viewer is not editable");
        }
        if !super::paste::accepts(text, self.search.query.len(), MAX_SEARCH_BYTES) {
            return Err("paste refused: search requires one line within 1024 bytes");
        }
        self.search.query.push_str(text);
        self.search.output_key = None;
        self.search.matches.clear();
        self.search.selected = 0;
        Ok(())
    }

    pub(super) fn key(
        &mut self,
        key: KeyEvent,
        details: &[Detail],
        live: &Live,
        status: &mut Status,
    ) -> Option<Action> {
        status.clear();
        if self.search.prompt {
            match key.code {
                KeyCode::Esc => self.search = Search::default(),
                KeyCode::Enter => {
                    self.search.prompt = false;
                    self.search.seek = true;
                    self.focus = Focus::Output;
                }
                KeyCode::Backspace => {
                    self.search.query.pop();
                    self.search.output_key = None;
                    self.search.matches.clear();
                    self.search.selected = 0;
                }
                KeyCode::Char(c)
                    if key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                        && !crate::terminal_text::unsafe_character(c)
                        && self.search.query.len() + c.len_utf8() <= MAX_SEARCH_BYTES =>
                {
                    self.search.query.push(c);
                    self.search.output_key = None;
                    self.search.matches.clear();
                    self.search.selected = 0;
                }
                _ => {}
            }
            return None;
        }

        let pane = match self.focus {
            Focus::Code => &mut self.code,
            Focus::Output => &mut self.output,
        };
        if pane.key(key) {
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                if !self.search.query.is_empty() {
                    self.search = Search::default();
                    return None;
                }
                return Some(Action::Close);
            }
            KeyCode::Char('q') => return Some(Action::Close),
            KeyCode::Char('/') if key.modifiers.is_empty() => {
                self.focus = Focus::Output;
                self.search = Search {
                    prompt: true,
                    ..Search::default()
                };
            }
            KeyCode::Char(direction @ ('n' | 'N'))
                if !self.search.matches.is_empty()
                    && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                    && (direction == 'N' || key.modifiers.is_empty()) =>
            {
                let count = self.search.matches.len();
                self.search.selected = if direction == 'N' {
                    self.search.selected.checked_sub(1).unwrap_or(count - 1)
                } else {
                    (self.search.selected + 1) % count
                };
                self.search.seek = true;
                self.focus = Focus::Output;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if !self.has_code {
                    self.focus = Focus::Output;
                    return None;
                }
                self.focus = match self.focus {
                    Focus::Code => Focus::Output,
                    Focus::Output => Focus::Code,
                };
            }
            KeyCode::Right | KeyCode::Left | KeyCode::Char('l' | 'h') => {
                let rows = selections(details, live);
                let at = rows
                    .iter()
                    .position(|key| *key == self.selected)
                    .unwrap_or(0);
                let at = if matches!(key.code, KeyCode::Right | KeyCode::Char('l')) {
                    (at + 1).min(rows.len().saturating_sub(1))
                } else {
                    at.saturating_sub(1)
                };
                if let Some(&selection) = rows.get(at) {
                    self.select(selection, details);
                }
            }
            KeyCode::Char('c') if key.modifiers.is_empty() => return self.copy(details, live),
            _ => {}
        }
        None
    }

    fn copy(&self, details: &[Detail], live: &Live) -> Option<Action> {
        let content = content(self.selected, details, live)?;
        let (text, what) = match self.focus {
            Focus::Code => (content.code, "the code"),
            Focus::Output => {
                let text = match self.selected {
                    Selection::Detail(sequence) if content.markdown => details
                        .iter()
                        .find(|detail| detail.sequence == sequence)
                        .map_or(content.output, |detail| Cow::Borrowed(&detail.output)),
                    _ => content.output,
                };
                (text, "the output")
            }
        };
        Some(Action::Copy {
            text: text.into_owned(),
            what,
        })
    }

    pub(super) fn mouse(&mut self, mouse: MouseEvent, stamp: Option<FrameStamp>) -> Option<String> {
        if stamp.is_none() || stamp != self.painted {
            return None;
        }
        let position = (mouse.column, mouse.row).into();
        let hit = if self.code.area.contains(position) {
            Some(Focus::Code)
        } else if self.output.area.contains(position) {
            Some(Focus::Output)
        } else {
            None
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                self.focus = hit?;
                self.search.prompt = false;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.focus = self.drag?.focus;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let drag = self.drag?;
                if !drag.moved {
                    return None;
                }
                let rows = match drag.focus {
                    Focus::Code => &self.code.lines,
                    Focus::Output => &self.output.lines,
                };
                return selection::text(rows, drag.range());
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.focus = hit?;
                let pane = match self.focus {
                    Focus::Code => &mut self.code,
                    Focus::Output => &mut self.output,
                };
                pane.key(KeyEvent::new(
                    if mouse.kind == MouseEventKind::ScrollUp {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    },
                    KeyModifiers::NONE,
                ));
                return None;
            }
            _ => return None,
        }
        let pane = match self.focus {
            Focus::Code => &mut self.code,
            Focus::Output => &mut self.output,
        };
        let area = pane.area;
        if area.height == 0 || area.width == 0 {
            return None;
        }
        if mouse.row < area.y {
            pane.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        if mouse.row >= area.bottom() {
            pane.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let row = pane.scroll + usize::from(mouse.row.clamp(area.y, area.bottom() - 1) - area.y);
        let rows = match self.focus {
            Focus::Code => &self.code.lines,
            Focus::Output => &self.output.lines,
        };
        let row = rows.get(row.min(rows.len().saturating_sub(1)))?;
        let point = point_at(row, usize::from(mouse.column.saturating_sub(area.x)));
        match &mut self.drag {
            Some(drag) => {
                drag.end = point;
                drag.moved = true;
            }
            None => {
                self.drag = Some(Drag {
                    focus: self.focus,
                    start: point,
                    end: point,
                    moved: false,
                })
            }
        }
        None
    }

    pub(super) fn prepare(
        &mut self,
        size: Size,
        details: &[Detail],
        live: &Live,
        status: &mut Status,
    ) {
        if self.size.width != size.width && !self.search.prompt && !self.search.query.is_empty() {
            self.search.seek = true;
        }
        self.size = size;
        let entries = selections(details, live);
        if !entries.contains(&self.selected)
            && let Some(&last) = entries.last()
        {
            self.select(last, details);
        }
        let Some(content) = content(self.selected, details, live) else {
            self.empty = true;
            return;
        };
        self.empty = false;
        self.has_code = !content.markdown && !content.code.is_empty();
        if !self.has_code {
            self.focus = Focus::Output;
        }
        let code = detail_excerpt::head(&content.code, MAX_CODE_BYTES);
        let code_excerpt = code.len() < content.code.len() || !content.code_complete;
        let cached = self
            .code_key
            .as_ref()
            .is_some_and(|(selected, width, code, review)| {
                *selected == self.selected
                    && *width == size.width
                    && *code == content.code.len()
                    && review.as_deref() == content.review
            });
        if !cached {
            self.code_key = Some((
                self.selected,
                size.width,
                content.code.len(),
                content.review.map(str::to_owned),
            ));
            let mut code_lines: Vec<_> = content
                .review
                .map(|review| Line::styled(review.to_owned(), Style::new().dim().italic()))
                .into_iter()
                .chain(super::highlight::python(&super::highlight::statements(
                    code,
                )))
                .collect();
            if code_excerpt {
                code_lines.push(Line::raw(code_notice(content.code_complete)));
            }
            self.code.lines = if self.has_code {
                code_lines
                    .into_iter()
                    .enumerate()
                    .flat_map(|(number, line)| {
                        selection::wrap_line(&line, number as u64, 0, usize::from(size.width))
                    })
                    .collect()
            } else {
                Vec::new()
            };
        }
        let query = self.search.query.to_ascii_lowercase();
        let (source_hash, output_hash) = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            content.markdown.hash(&mut hasher);
            content.output.hash(&mut hasher);
            let source_hash = hasher.finish();
            for highlight in content.highlights.iter() {
                highlight.range.hash(&mut hasher);
                highlight.style.hash(&mut hasher);
            }
            query.hash(&mut hasher);
            (source_hash, hasher.finish())
        };
        let key = (self.selected, size.width, output_hash);
        if self.search.output_key != Some(key) {
            let output = if content.markdown {
                super::markdown::render_detail(&content.output, size.width)
            } else {
                output_rows(&content, size.width)
            };
            if self
                .search
                .output_source
                .is_some_and(|hash| hash != source_hash)
                && let Some(drag) = self.drag.filter(|drag| drag.focus == Focus::Output)
                && !selection::unchanged(
                    &self.output.lines,
                    &output,
                    Some(drag.start),
                    Some(drag.end),
                )
            {
                self.drag = None;
            }
            self.search.output_key = Some(key);
            self.search.output_source = Some(source_hash);
            (self.search.matches, self.search.marks) = selection::search(&output, &query);
            self.output.lines = output;
        }
        if self.search.selected >= self.search.matches.len() {
            self.search.selected = 0;
        }
        let compact = size.height < 8;
        let inner = Rect::new(0, 1, size.width, size.height.saturating_sub(2));
        self.code.area = Rect::default();
        self.code.label = Rect::default();
        self.output.area = Rect::default();
        self.output.label = Rect::default();
        if !self.has_code {
            [self.output.label, self.output.area] =
                Layout::vertical([Constraint::Length(u16::from(!compact)), Constraint::Fill(1)])
                    .areas(inner);
        } else if compact {
            match self.focus {
                Focus::Code => self.code.area = inner,
                Focus::Output => self.output.area = inner,
            }
        } else {
            let code_rows = u16::try_from(self.code.lines.len())
                .unwrap_or(u16::MAX)
                .clamp(1, (inner.height / 3).max(3));
            let pad = u16::from(inner.height >= 10);
            [
                _,
                self.code.label,
                self.code.area,
                _,
                self.output.label,
                self.output.area,
            ] = Layout::vertical([
                Constraint::Length(pad),
                Constraint::Length(1),
                Constraint::Length(code_rows),
                Constraint::Length(pad),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(inner);
        }
        if std::mem::take(&mut self.search.seek) {
            if let Some(&row) = self.search.matches.get(self.search.selected) {
                self.output.anchor = Some(self.output.lines[row].anchor);
            } else if !query.is_empty() {
                status.set(
                    Kind::Info,
                    Clean::literal("No matches"),
                    Some(std::time::Duration::from_secs(3)),
                );
            }
        }
        self.code.place();
        self.output.place();
        let index = entries
            .iter()
            .position(|entry| *entry == self.selected)
            .unwrap_or(0);
        let state = if content.markdown {
            if content.state == Some(LiveState::Running) {
                " · live".to_owned()
            } else {
                String::new()
            }
        } else {
            content
                .state
                .map_or_else(String::new, |state| format!(" · live · {}", state.label()))
        };
        self.heading = Line::from(vec![
            Span::styled(" ▚▞ details ", Style::new().fg(brand()).bold()),
            Span::raw(format!(
                "{}/{}{state} · {} ",
                index + 1,
                entries.len(),
                content.title
            )),
        ]);
        self.output_heading = "OUTPUT".to_owned();
        if content.state == Some(LiveState::Running) {
            self.output_heading
                .push_str(if self.output.anchor.is_none() {
                    " · FOLLOWING"
                } else {
                    " · PAUSED"
                });
        }
        if !content.output_complete || content.ui_notice_truncated {
            self.output_heading.push_str(" · EXCERPT");
        }
        let (name, excerpt, paused) = match self.focus {
            Focus::Code => ("CODE", code_excerpt, self.code.anchor.is_some()),
            Focus::Output => (
                "OUT",
                !content.output_complete,
                self.output.anchor.is_some(),
            ),
        };
        self.compact_title = name.to_owned();
        for (show, word) in [
            (excerpt, "EXCERPT"),
            (
                paused && content.state == Some(LiveState::Running),
                "PAUSED",
            ),
            (content.state == Some(LiveState::Running), "LIVE"),
        ] {
            if show
                && text::width(&self.compact_title) + 1 + text::width(word)
                    <= usize::from(size.width)
            {
                self.compact_title.push(' ');
                self.compact_title.push_str(word);
            }
        }
        self.waiting = content.output.is_empty() && content.state == Some(LiveState::Running);
    }
    pub(super) fn draw(&self, frame: &mut Frame, status: &Status) {
        let [body, bar] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        if self.empty {
            frame.render_widget(Paragraph::new(" No details yet"), body);
        } else {
            let heading = if self.size.height < 8 {
                Line::raw(&self.compact_title)
            } else {
                selection::borrowed(&self.heading)
            };
            frame.render_widget(heading, Rect::new(0, 0, body.width, 1));
            for (focus, pane, heading, marks) in [
                (Focus::Code, &self.code, "CODE", &[][..]),
                (
                    Focus::Output,
                    &self.output,
                    &self.output_heading,
                    self.search.marks.as_slice(),
                ),
            ] {
                let range = self
                    .drag
                    .filter(|drag| drag.focus == focus && drag.moved)
                    .map(Drag::range);
                pane.draw(frame, heading, focus == self.focus, range, marks);
            }
            if self.waiting {
                frame.render_widget(Paragraph::new("[no output yet]"), self.output.area);
            }
        }
        let hint = if let Some(note) = status.text() {
            Line::raw(text::clip_end(
                &note.replace(['\n', '\r'], " "),
                usize::from(bar.width),
            ))
        } else if self.search.prompt {
            self.search_prompt(bar.width)
        } else {
            Line::styled(self.hint(bar.width), Style::new().dim())
        };
        frame.render_widget(hint, bar);
    }
    fn hint(&self, width: u16) -> String {
        if self.empty {
            return text::fit(
                [Item {
                    full: "Esc back".into(),
                    short: None,
                    priority: 1,
                }],
                usize::from(width),
            );
        }
        let mut hints = vec![if self.search.query.is_empty() {
            "Esc back".to_owned()
        } else {
            "Esc clear".to_owned()
        }];
        if !self.search.matches.is_empty() {
            hints.push(format!(
                "n/N find [{}/{}]",
                self.search.selected + 1,
                self.search.matches.len()
            ));
        }
        if self.has_code {
            hints.push("Tab switch focus".to_owned());
        }
        hints.extend(["c copy".to_owned(), "/ search output".to_owned()]);
        hints.extend(
            [
                "↑↓/PgUp/PgDn scroll",
                "←→ switch entry",
                "g top",
                "G follow",
            ]
            .map(str::to_owned),
        );
        text::fit(
            hints.into_iter().enumerate().map(|(i, full)| Item {
                full,
                short: None,
                priority: usize::MAX - i,
            }),
            usize::from(width),
        )
    }
    fn search_prompt(&self, width: u16) -> Line<'static> {
        let room = usize::from(width);
        let lead = text::clip_end(
            &format!("{} matches /", self.search.matches.len()),
            room.saturating_sub(2),
        );
        let available = room.saturating_sub(text::width(&lead) + 1);
        let hints = text::fit(
            [
                Item {
                    full: "Enter search output".into(),
                    short: None,
                    priority: 2,
                },
                Item {
                    full: "Esc cancel".into(),
                    short: None,
                    priority: 1,
                },
            ],
            available.saturating_sub(8),
        );
        let reserve = if hints.is_empty() {
            0
        } else {
            text::width(&hints) + 1
        };
        Line::from(vec![
            Span::raw(lead),
            Span::raw(text::clip_start(
                &self.search.query,
                available.saturating_sub(reserve),
            )),
            Span::styled(" ", Style::new().reversed()),
            Span::styled(
                if hints.is_empty() {
                    hints
                } else {
                    format!(" {hints}")
                },
                Style::new().dim(),
            ),
        ])
    }
}
fn pane_heading(name: &str, pane: &Pane, focused: bool) -> Line<'static> {
    let total = pane.lines.len();
    let first = (pane.scroll + 1).min(total);
    let last = (pane.scroll + usize::from(pane.area.height)).min(total);
    let marker = if focused { " ▶ " } else { " " };
    let text = text::fit(
        [Item {
            full: format!("{marker}{name} · lines {first}–{last} of {total} "),
            short: Some(format!(
                "{marker}{} {first}–{last}/{total} ",
                name.split(" · ").next().unwrap_or(name)
            )),
            priority: 100,
        }],
        usize::from(pane.label.width).saturating_sub(4),
    );
    let (label, rule) = if focused {
        (Style::new().bold(), Style::new().fg(brand()))
    } else {
        (Style::new().bold(), Style::new().dim())
    };
    let rest = usize::from(pane.label.width).saturating_sub(text::width(&text) + 4);
    Line::from(vec![
        Span::styled("── ", rule),
        Span::styled(text, label),
        Span::styled(format!(" {}", "─".repeat(rest)), rule),
    ])
}
fn output_rows(content: &Content<'_>, width: u16) -> Vec<Row> {
    let mut offset = 0;
    let mut highlights = content.highlights.iter().peekable();
    content
        .output
        .split_inclusive('\n')
        .map(|segment| {
            let line = segment.trim_end_matches(['\n', '\r']);
            let line_end = offset + line.len();
            let mut spans = Vec::new();
            let mut line_bg = None;
            let mut at = offset;
            while at < line_end {
                while highlights.peek().is_some_and(|span| span.range.end <= at) {
                    highlights.next();
                }
                let (to, style) = match highlights.peek() {
                    Some(span) if span.range.start <= at => {
                        if span.range.start <= offset && span.range.end >= line_end {
                            line_bg = line_bg.or(span.style.bg);
                        }
                        (line_end.min(span.range.end), span.style)
                    }
                    Some(span) => (line_end.min(span.range.start), Style::new()),
                    None => (line_end, Style::new()),
                };
                spans.push(Span::styled(
                    line[at - offset..to - offset].to_owned(),
                    style,
                ));
                at = to;
            }
            if spans.is_empty() {
                while highlights
                    .peek()
                    .is_some_and(|span| span.range.end <= offset)
                {
                    highlights.next();
                }
                if let Some(span) = highlights
                    .peek()
                    .filter(|span| span.range.start <= offset && span.range.end > offset)
                {
                    line_bg = span.style.bg;
                }
            }
            offset += segment.len();
            Line::from(spans).style(Style {
                bg: line_bg,
                ..Style::new()
            })
        })
        .enumerate()
        .flat_map(|(number, line)| {
            selection::wrap_line(&line, number as u64, 0, usize::from(width))
        })
        .collect()
}

fn highlighted<'a>(line: &'a Line<'_>, ranges: &[Range<usize>]) -> Line<'a> {
    use unicode_segmentation::UnicodeSegmentation as _;

    let mut offset = 0;
    let mut matches = ranges.iter().peekable();
    let mut spans = Vec::new();
    for span in &line.spans {
        spans.extend(span.content.graphemes(true).map(|glyph| {
            let start = offset;
            offset += glyph.len();
            while matches.peek().is_some_and(|range| range.end <= start) {
                matches.next();
            }
            let style = if matches.peek().is_some_and(|range| range.start < offset) {
                span.style.bold().reversed()
            } else {
                span.style
            };
            Span::styled(glyph, style)
        }));
    }
    Line {
        spans,
        style: line.style,
        alignment: line.alignment,
    }
}

fn selections(details: &[Detail], live: &Live) -> Vec<Selection> {
    let mut jobs = live.jobs();
    jobs.retain(|job| {
        job.state == LiveState::Running
            || !details.iter().any(|detail| {
                detail
                    .terminal_job_id()
                    .is_some_and(|id| job.job == Some(id))
            })
    });
    let mut rows: Vec<_> = details
        .iter()
        .map(|detail| {
            (
                Selection::Detail(detail.sequence),
                detail.order_hint.or(detail.live_run_id),
            )
        })
        .collect();
    rows.sort_by_key(|(_, order)| order.unwrap_or(0));
    let mut finished: Vec<_> = jobs
        .iter()
        .filter(|job| job.state != LiveState::Running)
        .collect();
    finished.sort_unstable_by_key(|job| job.order);
    for job in finished {
        let at = rows
            .iter()
            .position(|(_, order)| order.is_some_and(|order| order > job.order))
            .unwrap_or(rows.len());
        rows.insert(at, (Selection::Job(job.id), Some(job.order)));
    }
    rows.extend(
        jobs.iter()
            .filter(|job| job.state == LiveState::Running)
            .map(|job| (Selection::Job(job.id), Some(job.order))),
    );
    rows.into_iter().map(|(selection, _)| selection).collect()
}

fn is_ui_notice(selected: Selection, details: &[Detail]) -> bool {
    match selected {
        Selection::Detail(sequence) => details
            .iter()
            .any(|detail| detail.sequence == sequence && detail.ui_notice_truncated().is_some()),
        Selection::Job(_) => false,
    }
}

fn content<'a>(selected: Selection, details: &'a [Detail], live: &Live) -> Option<Content<'a>> {
    match selected {
        Selection::Detail(index) => {
            let detail = details.iter().find(|detail| detail.sequence == index)?;
            if let Some(id) = detail.live_run_id
                && let Some(snapshot) = live.snapshot(id)
                && snapshot.summary.state == LiveState::Running
            {
                let mut content = from_snapshot(snapshot);
                content.review = detail.review.as_deref();
                content.code = Cow::Borrowed(&detail.code);
                content.code_complete = true;
                return Some(content);
            }
            let (output, output_complete, highlights) =
                output_excerpt(&detail.output, true, &detail.highlights);
            Some(Content {
                title: detail.title.chars().take(160).collect(),
                review: detail.review.as_deref(),
                code: Cow::Borrowed(&detail.code),
                code_complete: true,
                output,
                output_complete: output_complete && detail.ui_notice_truncated() != Some(true),
                ui_notice_truncated: detail.ui_notice_truncated() == Some(true),
                highlights,
                state: detail.compaction_running().then_some(LiveState::Running),
                markdown: detail.is_compaction(),
            })
        }
        Selection::Job(id) => live.snapshot(id).map(from_snapshot),
    }
}
fn from_snapshot(snapshot: Snapshot) -> Content<'static> {
    let mut output = Clean::new(&snapshot.output).to_string();
    let mut highlights = Vec::new();
    if let Some(path) = snapshot.saved {
        let target = super::links::file(&path);
        output.push_str("\n[saved printed output: ");
        let start = output.len();
        output.push_str(&Clean::new(&path.display().to_string()));
        let end = output.len();
        output.push(']');
        let style = super::links::style(Style::new(), target);
        if style != Style::new() {
            highlights.push(Highlight {
                range: start..end,
                style,
            });
        }
    }
    let (output, output_complete, highlights) =
        output_excerpt(&output, snapshot.output_complete, &highlights);
    Content {
        title: Clean::new(&snapshot.summary.title).to_string(),
        review: None,
        code: Clean::new(&snapshot.code).to_string().into(),
        code_complete: snapshot.code_complete,
        output: output.into_owned().into(),
        output_complete,
        ui_notice_truncated: false,
        highlights: highlights.into_owned().into(),
        state: Some(snapshot.summary.state),
        markdown: false,
    }
}

fn code_notice(complete: bool) -> &'static str {
    if complete {
        "[code excerpt; c copies the complete recorded code]"
    } else {
        "[code excerpt; c copies only the retained excerpt]"
    }
}

fn output_excerpt<'a>(
    text: &'a str,
    complete: bool,
    highlights: &'a [Highlight],
) -> (Cow<'a, str>, bool, Cow<'a, [Highlight]>) {
    if complete && text.len() <= MAX_OUTPUT_BYTES {
        return (Cow::Borrowed(text), true, Cow::Borrowed(highlights));
    }
    const NOTICE: &str = "[output excerpt; beginning omitted]\n";
    let tail = detail_excerpt::tail(text, MAX_OUTPUT_BYTES - NOTICE.len());
    let omitted = text.len() - tail.len();
    let highlights = highlights
        .iter()
        .filter(|span| span.range.end > omitted)
        .map(|span| Highlight {
            range: span.range.start.saturating_sub(omitted) + NOTICE.len()
                ..span.range.end - omitted + NOTICE.len(),
            style: span.style,
        })
        .collect();
    (
        Cow::Owned(format!("{NOTICE}{tail}")),
        false,
        Cow::Owned(highlights),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(
        viewer: &mut Viewer,
        kind: MouseEventKind,
        (column, row): (u16, u16),
    ) -> Option<String> {
        let stamp = Some(FrameStamp {
            seq: 1,
            size: (viewer.size.width, viewer.size.height),
        });
        viewer.painted(stamp);
        viewer.mouse(
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            stamp,
        )
    }
    fn locate(viewer: &Viewer, token: &str) -> (u16, u16) {
        let pane = &viewer.output;
        pane.lines
            .iter()
            .skip(pane.scroll)
            .enumerate()
            .find_map(|(row, projected)| {
                let rendered = projected.line().to_string();
                rendered.find(token).map(|byte| {
                    (
                        pane.area.x + u16::try_from(text::width(&rendered[..byte])).unwrap(),
                        pane.area.y + u16::try_from(row).unwrap(),
                    )
                })
            })
            .expect(token)
    }

    #[test]
    fn live_plain_output_append_keeps_mouse_drag() {
        let runtime = std::sync::Arc::new(Live::default());
        let mut job = runtime.start("test", "", Some(1));
        job.result("retained line\n");
        let mut viewer = Viewer::new(&[], &runtime, true);
        let size = Size::new(30, 16);
        let mut status = Status::default();
        viewer.prepare(size, &[], &runtime, &mut status);
        let (column, row) = locate(&viewer, "retained line");
        let button = MouseButton::Left;
        mouse(&mut viewer, MouseEventKind::Down(button), (column, row));
        mouse(
            &mut viewer,
            MouseEventKind::Drag(button),
            (column + 12, row),
        );
        job.result("a new line\n");
        viewer.prepare(size, &[], &runtime, &mut status);
        locate(&viewer, "a new line");
        assert_eq!(
            mouse(&mut viewer, MouseEventKind::Up(button), (column, row)).as_deref(),
            Some("retained line")
        );
    }

    #[test]
    fn live_viewer_discards_a_drag_before_reusing_table_offsets() {
        let mut transcript = crate::tui::transcript::Transcript::default();
        let runtime = Live::default();
        let mut status = Status::default();
        transcript.apply(super::super::session::UiEvent::Compacting, 24);
        transcript.apply(
            super::super::session::UiEvent::CompactionText(
                "| Key | Data |\n| --- | --- |\n| x | ab |".into(),
            ),
            24,
        );
        let mut viewer = Viewer::new(transcript.details(), &runtime, true);
        let size = Size::new(24, 16);
        viewer.prepare(size, transcript.details(), &runtime, &mut status);
        let (column, row) = locate(&viewer, "ab");
        let button = MouseButton::Left;
        mouse(&mut viewer, MouseEventKind::Down(button), (column + 1, row));
        mouse(&mut viewer, MouseEventKind::Drag(button), (column, row));
        transcript.apply(
            super::super::session::UiEvent::Compacted(
                "| Key | Data |\n| --- | --- |\n| x | 界 |".into(),
            ),
            24,
        );
        viewer.prepare(size, transcript.details(), &runtime, &mut status);
        assert_eq!(
            mouse(&mut viewer, MouseEventKind::Up(button), (column, row)),
            None
        );
        let wide = locate(&viewer, "界");
        mouse(&mut viewer, MouseEventKind::Down(button), wide);
        mouse(
            &mut viewer,
            MouseEventKind::Drag(button),
            (wide.0 + 1, wide.1),
        );
        assert_eq!(
            mouse(&mut viewer, MouseEventKind::Up(button), wide).as_deref(),
            Some("界")
        );
    }

    #[test]
    fn partial_grapheme_search_preserves_painted_columns_and_mouse_copy() {
        use ratatui::style::Modifier;

        for (glyph, query, width) in [("👩‍💻", "👩", 2), ("e\u{301}", "e", 1)] {
            let mut transcript = crate::tui::transcript::Transcript::default();
            transcript.compaction_note(&format!("> {glyph}xy"));
            let runtime = Live::default();
            let details = transcript.details();
            let mut viewer = Viewer::new(details, &runtime, true);
            let mut status = Status::default();
            viewer.key(
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                details,
                &runtime,
                &mut status,
            );
            viewer.paste(query).unwrap();
            viewer.prepare(Size::new(18, 12), details, &runtime, &mut status);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(18, 12)).unwrap();
            terminal.draw(|frame| viewer.draw(frame, &status)).unwrap();
            let buffer = terminal.backend().buffer();
            let (column, row) = locate(&viewer, glyph);
            assert_eq!(buffer[(column, row)].symbol(), glyph);
            assert!(buffer[(column, row)].modifier.contains(Modifier::REVERSED));
            let after = column + width;
            assert_eq!(buffer[(after, row)].symbol(), "x");
            assert!(!buffer[(after, row)].modifier.contains(Modifier::REVERSED));
            let button = MouseButton::Left;
            mouse(&mut viewer, MouseEventKind::Down(button), (after, row));
            mouse(&mut viewer, MouseEventKind::Drag(button), (after + 1, row));
            assert_eq!(
                mouse(&mut viewer, MouseEventKind::Up(button), (after + 1, row)).as_deref(),
                Some("xy")
            );
        }
    }

    #[test]
    fn table_search_counts_cells_not_wrapped_fragments_or_labels() {
        let mut transcript = crate::tui::transcript::Transcript::default();
        transcript.compaction_note(
            "| Key | Data |\n| --- | --- |\n| x | abcdefg   hij klm |\n| y | other |",
        );
        let runtime = Live::default();
        let details = transcript.details();
        for (query, expected) in [("data", "Data"), ("abcdefg   hij", "abcdefghij")] {
            let mut viewer = Viewer::new(details, &runtime, true);
            let mut status = Status::default();
            viewer.key(
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                details,
                &runtime,
                &mut status,
            );
            viewer.paste(query).unwrap();
            for width in [32, 21, 16] {
                viewer.prepare(Size::new(width, 18), details, &runtime, &mut status);
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 18)).unwrap();
                terminal.draw(|frame| viewer.draw(frame, &status)).unwrap();
                let buffer = terminal.backend().buffer();
                let marked: String = (viewer.output.area.y..viewer.output.area.bottom())
                    .flat_map(|row| (0..width).map(move |column| (column, row)))
                    .map(|at| &buffer[at])
                    .filter(|cell| cell.modifier.contains(ratatui::style::Modifier::REVERSED))
                    .map(ratatui::buffer::Cell::symbol)
                    .filter(|glyph| *glyph != " ")
                    .collect();
                assert_eq!(marked, expected);
                let painted: String = buffer
                    .content
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect();
                assert!(painted.contains("1 match") || painted.contains("1/1"));
            }
        }
    }

    #[test]
    fn editing_back_to_the_painted_query_preserves_count_and_navigation() {
        let mut transcript = crate::tui::transcript::Transcript::default();
        transcript.note("test", "first needle\nsecond needle");
        let details = transcript.details();
        let live = Live::default();
        let mut viewer = Viewer::new(details, &live, true);
        let mut status = Status::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        let mut draw = |v: &mut Viewer, s: &mut Status| {
            v.prepare(Size::new(80, 12), details, &live, s);
            terminal.draw(|frame| v.draw(frame, s)).unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        let key = |v: &mut Viewer, s: &mut Status, code| {
            v.key(KeyEvent::new(code, KeyModifiers::NONE), details, &live, s);
        };
        key(&mut viewer, &mut status, KeyCode::Char('/'));
        viewer.paste("needle").unwrap();
        assert!(draw(&mut viewer, &mut status).contains("2 matches /needle"));
        key(&mut viewer, &mut status, KeyCode::Char('x'));
        key(&mut viewer, &mut status, KeyCode::Backspace);
        assert!(draw(&mut viewer, &mut status).contains("2 matches /needle"));
        key(&mut viewer, &mut status, KeyCode::Enter);
        key(&mut viewer, &mut status, KeyCode::Char('n'));
        assert!(draw(&mut viewer, &mut status).contains("[2/2]"));
    }

    #[test]
    fn viewer_search_finds_text_across_a_soft_wrap() {
        let mut transcript = crate::tui::transcript::Transcript::default();
        transcript.compaction_note("> First alpha beta gamma");
        let live = Live::default();
        let details = transcript.details();
        let mut viewer = Viewer::new(details, &live, true);
        let mut status = Status::default();
        viewer.key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            details,
            &live,
            &mut status,
        );
        viewer.paste("alpha beta").unwrap();
        viewer.prepare(Size::new(14, 12), details, &live, &mut status);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(14, 12)).unwrap();
        terminal.draw(|frame| viewer.draw(frame, &status)).unwrap();
        let buffer = terminal.backend().buffer();
        let area = viewer.output.area;
        let selected: String = (area.y..area.bottom())
            .flat_map(|row| (area.x..area.right()).map(move |col| (col, row)))
            .map(|at| &buffer[at])
            .filter(|cell| cell.modifier.contains(ratatui::style::Modifier::REVERSED))
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert_eq!(selected, "alpha beta");
    }

    #[test]
    fn compaction_summary_mouse_copy_survives_reflow() {
        let mut transcript = crate::tui::transcript::Transcript::default();
        transcript.compaction_note("> Summary\n\n| Name | Data |\n| --- | ---: |\n| x | abcdefg   hij klm |\n\n```\n    print(value)\n```");
        let details = transcript.details();
        let live = Live::default();
        let mut viewer = Viewer::new(details, &live, true);
        let mut status = Status::default();
        viewer.prepare(Size::new(21, 40), details, &live, &mut status);
        let first = locate(&viewer, "abcdefg");
        let last = locate(&viewer, "klm");
        let button = MouseButton::Left;
        mouse(&mut viewer, MouseEventKind::Down(button), first);
        mouse(
            &mut viewer,
            MouseEventKind::Drag(button),
            (last.0 + 2, last.1),
        );
        viewer.prepare(Size::new(14, 40), details, &live, &mut status);
        let last = locate(&viewer, "klm");
        assert_eq!(
            mouse(&mut viewer, MouseEventKind::Up(button), last).as_deref(),
            Some("abcdefg   hij klm")
        );
        let start = locate(&viewer, "Summary");
        let last = locate(&viewer, "e)");
        mouse(&mut viewer, MouseEventKind::Down(button), (0, start.1));
        mouse(
            &mut viewer,
            MouseEventKind::Drag(button),
            (last.0 + 1, last.1),
        );
        assert_eq!(
            mouse(&mut viewer, MouseEventKind::Up(button), last).as_deref(),
            Some(
                "Summary\n\n| Name | Data |\n| --- | ---: |\n| x | abcdefg   hij klm |\n\n    print(value)"
            )
        );
    }

    #[test]
    fn link_highlights_do_not_bleed_into_trailing_cells_and_saved_paths_stay_readable() {
        let dir = tempfile::tempdir().unwrap();
        let saved = dir.path().join("my output.txt");
        std::fs::write(&saved, "ok\n").unwrap();

        let make_snapshot = || Snapshot {
            summary: crate::tool::LiveSummary {
                id: 1,
                job: None,
                title: "run".into(),
                state: LiveState::Finished,
                order: 1,
            },
            code: String::new(),
            code_complete: true,
            output: "hello".into(),
            output_complete: true,
            saved: Some(saved.clone()),
        };

        let disabled = super::super::links::with_enabled(false, || from_snapshot(make_snapshot()));
        assert!(
            disabled
                .output
                .contains(&format!("[saved printed output: {}]", saved.display()))
        );
        assert!(!disabled.output.contains("file://"));
        assert!(disabled.highlights.is_empty());

        super::super::links::with_enabled(true, || {
            let content = from_snapshot(make_snapshot());
            assert!(
                content
                    .output
                    .contains(&format!("[saved printed output: {}]", saved.display()))
            );
            assert_eq!(content.highlights.len(), 1);
            assert_eq!(
                &content.output[content.highlights[0].range.clone()],
                saved.display().to_string()
            );

            let mut transcript = crate::tui::transcript::Transcript::default();
            transcript.apply(crate::tui::session::UiEvent::Image(saved.clone()), 80);
            let details = transcript.details();
            let live = Live::default();
            let mut viewer = Viewer::new(details, &live, true);
            let mut status = Status::default();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
            viewer.prepare(Size::new(80, 10), details, &live, &mut status);
            terminal
                .draw(|frame| {
                    viewer.draw(frame, &status);
                    super::super::links::paint(frame.buffer_mut());
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let row = viewer.output.area.y;
            assert!(buf[(0, row)].symbol().contains("\x1b]8;id=bart"));
            assert!(
                !buf[(79, row)].symbol().contains("\x1b]8;"),
                "trailing blank cell must not be wrapped in OSC 8"
            );
            assert!(
                !buf[(79, row)]
                    .modifier
                    .contains(ratatui::style::Modifier::UNDERLINED),
                "trailing blank cell must not be underlined"
            );
        });
    }
}
