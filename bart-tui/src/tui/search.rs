use super::{
    brand,
    selection::{self, Hit, Point, Row},
    status::{Kind, Status},
    text::{self, Clean, Item},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::{borrow::Cow, ops::Range, time::Duration};
use unicode_segmentation::UnicodeSegmentation as _;

const MAX_SEARCH_BYTES: usize = 1024;

pub(super) enum PromptKey {
    Unhandled,
    Consumed,
    Submitted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Inactive,
    Prompt,
    Active,
}

#[derive(Default)]
pub(super) struct Search {
    mode: Mode,
    query: String,
    lower: String,
    indexed_query: String,
    matches: Vec<Hit>,
    selected: usize,
    seek: Option<isize>,
}

impl Search {
    pub(super) fn open(&mut self) {
        *self = Self {
            mode: Mode::Prompt,
            ..Self::default()
        };
    }

    pub(super) fn in_mode(&self) -> bool {
        self.mode != Mode::Inactive
    }

    pub(super) fn prompting(&self) -> bool {
        self.mode == Mode::Prompt
    }

    pub(super) fn active(&self) -> bool {
        !self.query.is_empty()
    }

    pub(super) fn lower(&self) -> &str {
        &self.lower
    }

    pub(super) fn dismiss_prompt(&mut self) {
        if self.mode == Mode::Prompt {
            self.mode = if self.query.is_empty() {
                Mode::Inactive
            } else {
                Mode::Active
            };
        }
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn clear_key(&mut self, key: KeyEvent) -> bool {
        if key.code != KeyCode::Esc || !self.in_mode() {
            return false;
        }
        self.clear();
        true
    }

    pub(super) fn prompt_key(&mut self, key: KeyEvent) -> PromptKey {
        if self.mode != Mode::Prompt {
            return PromptKey::Unhandled;
        }
        match key.code {
            KeyCode::Esc => self.clear(),
            KeyCode::Enter => {
                if self.query.is_empty() {
                    self.clear();
                } else {
                    self.mode = Mode::Active;
                    self.request_seek();
                }
                return PromptKey::Submitted;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.lower.pop();
                self.selected = 0;
            }
            KeyCode::Char(c)
                if key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                    && !crate::terminal_text::unsafe_character(c)
                    && self.query.len() + c.len_utf8() <= MAX_SEARCH_BYTES =>
            {
                self.query.push(c);
                self.lower.push(c.to_ascii_lowercase());
                self.selected = 0;
            }
            _ => {}
        }
        PromptKey::Consumed
    }

    pub(super) fn paste(&mut self, text: &str) -> Result<(), &'static str> {
        if !super::paste::accepts(text, self.query.len(), MAX_SEARCH_BYTES) {
            return Err("paste refused: search requires one line within 1024 bytes");
        }
        self.query.push_str(text);
        self.lower
            .extend(text.chars().map(|c| c.to_ascii_lowercase()));
        self.selected = 0;
        Ok(())
    }

    pub(super) fn step(&mut self, key: KeyEvent) -> bool {
        let delta = match key.code {
            KeyCode::Char('N') if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => -1,
            KeyCode::Char('n') if key.modifiers.is_empty() => 1,
            _ => return false,
        };
        self.step_by(delta)
    }

    pub(super) fn step_by(&mut self, delta: isize) -> bool {
        if self.indexed() {
            let count = self.matches.len();
            if count == 0 {
                return false;
            }
            self.selected = offset_index(self.selected, count, delta);
            self.request_seek();
        } else if self.active() {
            let pending = self.seek.get_or_insert(0);
            *pending = pending.saturating_add(delta);
        } else {
            return false;
        }
        true
    }

    pub(super) fn indexed(&self) -> bool {
        self.indexed_query == self.lower
    }

    pub(super) fn matches(&self) -> &[Hit] {
        if self.indexed() { &self.matches } else { &[] }
    }

    pub(super) fn update_matches(
        &mut self,
        keep: usize,
        matches: Vec<Hit>,
        preferred: impl FnOnce(&[Hit]) -> Option<usize>,
    ) {
        self.indexed_query.clone_from(&self.lower);
        self.matches.truncate(keep);
        self.matches.extend(matches);
        self.selected = preferred(&self.matches).unwrap_or(self.selected);
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
        if let Some(pending) = &mut self.seek {
            let delta = std::mem::take(pending);
            if !self.matches.is_empty() {
                self.selected = offset_index(self.selected, self.matches.len(), delta);
            }
        }
    }

    pub(super) fn marks(&self, row: &Row) -> Vec<Range<usize>> {
        selection::marks(row, self.matches(), self.lower.len())
    }

    pub(super) fn target(&self) -> Option<Hit> {
        self.matches().get(self.selected).copied()
    }

    pub(super) fn request_seek(&mut self) {
        if self.active() {
            self.seek.get_or_insert(0);
        }
    }

    pub(super) fn has_pending_seek(&self) -> bool {
        self.seek.is_some()
    }

    pub(super) fn cancel_seek(&mut self) {
        self.seek = None;
    }

    pub(super) fn take_seek(&mut self, status: &mut Status) -> Option<Hit> {
        self.seek.take()?;
        let hit = self.target();
        if hit.is_none() && self.active() {
            status.set(
                Kind::Info,
                Clean::literal("No matches"),
                Some(Duration::from_secs(3)),
            );
        }
        hit
    }

    pub(super) fn step_hint(&self) -> Option<String> {
        self.target()
            .map(|_| format!("n/N find [{}/{}]", self.selected + 1, self.matches.len()))
    }

    pub(super) fn prompt_line(&self, width: u16, enter: &str) -> Option<Line<'static>> {
        if self.mode != Mode::Prompt {
            return None;
        }
        let room = usize::from(width);
        let lead = text::clip_end(
            &format!("{} matches /", self.matches().len()),
            room.saturating_sub(2),
        );
        let available = room.saturating_sub(text::width(&lead) + 1);
        let hints = text::fit(
            [
                Item {
                    full: enter.into(),
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
        Some(Line::from(vec![
            Span::raw(lead),
            Span::raw(text::clip_start(
                &self.query,
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
        ]))
    }

    pub(super) fn nav_line(&self, width: u16) -> Option<Line<'static>> {
        if self.mode != Mode::Active {
            return None;
        }
        let room = usize::from(width);
        let count = if self.matches().is_empty() {
            "0 matches".to_owned()
        } else {
            format!("[{}/{}]", self.selected + 1, self.matches().len())
        };
        let label = format!("find /{} {count}", self.query);
        let lead = text::clip_end(&label, room);
        let remaining = room.saturating_sub(text::width(&lead) + 2);
        let hints = text::fit(
            [
                Item {
                    full: "n/Enter next".into(),
                    short: Some("n next".into()),
                    priority: 4,
                },
                Item {
                    full: "N prev".into(),
                    short: None,
                    priority: 3,
                },
                Item {
                    full: "/ edit".into(),
                    short: None,
                    priority: 2,
                },
                Item {
                    full: "Esc exit".into(),
                    short: Some("Esc".into()),
                    priority: 5,
                },
            ],
            remaining,
        );
        let mut spans = vec![Span::styled(
            lead,
            Style::new().fg(brand()).add_modifier(Modifier::BOLD),
        )];
        if !hints.is_empty() {
            spans.push(Span::styled(format!("  {hints}"), Style::new().dim()));
        }
        Some(Line::from(spans))
    }
}

fn offset_index(index: usize, count: usize, delta: isize) -> usize {
    let offset = delta.unsigned_abs() % count;
    if delta < 0 {
        (index + count - offset) % count
    } else {
        (index + offset) % count
    }
}

pub(super) fn paint<'a>(
    row: &'a Row,
    drag: Option<(Point, Point)>,
    search: Option<&Search>,
) -> Line<'a> {
    let line = match drag {
        Some(range) => selection::highlight(row, range),
        None => selection::borrowed(row.line()),
    };
    match search {
        Some(search) => highlighted(line, &search.marks(row)),
        None => line,
    }
}

fn highlighted<'a>(mut line: Line<'a>, ranges: &[Range<usize>]) -> Line<'a> {
    if ranges.is_empty() {
        return line;
    }
    let mut offset = 0;
    let mut matches = ranges.iter().peekable();
    let mut style_at = |glyph: &str, style: Style| {
        let start = offset;
        offset += glyph.len();
        while matches.peek().is_some_and(|range| range.end <= start) {
            matches.next();
        }
        if matches.peek().is_some_and(|range| range.start < offset) {
            style.bold().reversed()
        } else {
            style
        }
    };
    let mut spans = Vec::new();
    for span in line.spans {
        let style = span.style;
        match span.content {
            Cow::Borrowed(text) => spans.extend(
                text.graphemes(true)
                    .map(|glyph| Span::styled(glyph, style_at(glyph, style))),
            ),
            Cow::Owned(text) => spans.extend(
                text.graphemes(true)
                    .map(|glyph| Span::styled(glyph.to_owned(), style_at(glyph, style))),
            ),
        }
    }
    line.spans = spans;
    line
}
