use crate::tui::text::clip_end;
use crate::tui::text::{Item, fit};
use std::{fmt::Write as _, io};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{App, MAX_COMPOSER_ROWS, Surface, parse_slash};
use crate::tui::SPIN;
use crate::tui::{Screen, brand, theme};

#[derive(Clone, Copy)]
enum HintContext {
    Mention,
    SlashMention,
    Slash,
    Command,
    Normal,
}

pub(in crate::tui) use crate::tui::widgets::confirm::FrameStamp;

pub(in crate::tui) struct Painted {
    pub(super) stamp: FrameStamp,
    pub(super) review: bool,
    pub(super) spinner_visible: bool,
}

impl App {
    pub(super) fn render_frame(
        &mut self,
        screen: &mut Screen,
        running_jobs: usize,
    ) -> io::Result<Painted> {
        let mut reviewable = false;
        let mut spinner_visible = false;
        let completed = screen.draw(|frame| {
            if let Some(Surface::Viewer(viewer, _)) = &self.surface {
                viewer.draw(frame, &self.status);
            } else {
                reviewable = self.draw_frame(frame, running_jobs);
            }
            spinner_visible = !matches!(self.surface, Some(Surface::Picker(_)))
                && (self.turn.is_some() || running_jobs > 0)
                && frame
                    .buffer_mut()
                    .content
                    .iter()
                    .any(|cell| SPIN.contains(&cell.symbol()));
        })?;
        let stamp = FrameStamp {
            seq: self.paint.seq + 1,
            size: (completed.area.width, completed.area.height),
        };
        let review = reviewable;
        Ok(Painted {
            stamp,
            review,
            spinner_visible,
        })
    }

    fn draw_frame(&mut self, frame: &mut Frame, running_jobs: usize) -> bool {
        let feedback = self.status.text().map(|text| {
            (
                text,
                match self.status.kind() {
                    super::StatusKind::Info => brand(),
                    super::StatusKind::Warning => theme().code,
                    super::StatusKind::Error => theme().error,
                },
            )
        });
        if let Some(Surface::Picker(picker)) = &self.surface {
            let area = crate::tui::picker::content_area(frame.area());
            if let Some(confirm) = picker.confirm() {
                return confirm.draw(frame, area, feedback);
            }
            if let Some(field) = picker.field() {
                field.draw(frame, area, feedback);
            } else {
                picker.menu().draw(frame, area, feedback);
            }
            return false;
        }
        if let Some(Surface::Approval(answer)) = &self.surface
            && answer.answer.pending()
        {
            let area = frame.area();
            let feedback = feedback.map(|(text, color)| (text.to_owned(), color));
            let panel = answer.confirm.panel(area, feedback.is_some());
            if let Some(panel) = panel {
                self.draw_live(
                    frame,
                    Rect {
                        height: panel.y - area.y,
                        ..area
                    },
                );
            }
            let Some(Surface::Approval(answer)) = &self.surface else {
                return false;
            };
            return answer.confirm.draw(
                frame,
                area,
                feedback
                    .as_ref()
                    .map(|(text, color)| (text.as_str(), *color)),
            );
        }
        let lead = 2;
        let room = usize::from(frame.area().width.saturating_sub(lead + 1)).max(1);
        let shown = self.composer_rows(frame.area().width);
        let hint_rows = u16::from(
            self.turn.is_some()
                || self.status.text().is_some()
                || usize::from(frame.area().height) >= shown + 6,
        );
        let top_rows = frame.area().height.min(2);
        let height = frame.area().height - top_rows;
        self.draw_top_bar(frame);
        let shown = u16::try_from(shown).unwrap_or(1);
        let decorated = height >= shown.saturating_add(4 + hint_rows);
        let status_rows = u16::from(height > 1);
        let hint_rows = hint_rows.min(height.saturating_sub(status_rows + 1));
        let draft_rows = shown
            .min(height.saturating_sub(status_rows + hint_rows))
            .max(u16::from(height > 0));
        let borders = u16::from(decorated) * 2;
        let spacer = u16::from(decorated);
        let available =
            height.saturating_sub(draft_rows + borders + spacer + status_rows + hint_rows);
        let pending_rows =
            u16::try_from((self.drafts.steers.len() + self.drafts.queued.len()).min(3))
                .expect("at most three rows")
                .min(available);
        let mut y = frame.area().y + top_rows;
        let [live, _, pending, composer, status, hints] = [
            available - pending_rows,
            spacer,
            pending_rows,
            draft_rows + borders,
            status_rows,
            hint_rows,
        ]
        .map(|height| {
            let area = Rect {
                y,
                height,
                ..frame.area()
            };
            y += height;
            area
        });
        let hint_context = self.draw_live(frame, live);

        frame.render_widget(
            Paragraph::new(self.pending_lines(pending.width, pending.height)),
            pending,
        );
        let (rows, cursor) = self.drafts.composer.layout(room);
        draw_composer(
            frame,
            composer,
            rows,
            cursor,
            lead,
            decorated,
            self.info.name.as_deref(),
        );

        let count = self.drafts.steers.len() + self.drafts.queued.len();
        let pending = if pending_rows == 0 { count } else { 0 };
        let left = self.status_left(running_jobs, usize::from(status.width), pending);
        let left_width = left.width().min(usize::from(status.width));
        let right_x =
            left_width.saturating_add(usize::from(left_width < usize::from(status.width)));
        let right_width = usize::from(status.width).saturating_sub(right_x);
        let left_area = Rect {
            width: left_width as u16,
            ..status
        };
        let right_area = Rect {
            x: status.x.saturating_add(right_x as u16),
            width: right_width as u16,
            ..status
        };
        frame.render_widget(Paragraph::new(left), left_area);
        frame.render_widget(
            Paragraph::new(self.usage_line(right_width).right_aligned()),
            right_area,
        );
        if hint_rows > 0 {
            let hint = match self.status.text() {
                Some(text) => Line::from(Span::styled(
                    clip_end(&format!("  {text}"), usize::from(hints.width)),
                    Style::new().fg(brand()),
                )),
                None => self.hint_line(usize::from(hints.width), hint_context),
            };
            frame.render_widget(Paragraph::new(hint), hints);
        }
        false
    }

    fn draw_top_bar(&self, frame: &mut Frame) {
        let area = Rect {
            height: frame.area().height.min(1),
            ..frame.area()
        };
        let width = usize::from(area.width);
        let mut model = self.info.settings.model.clone();
        if let Some(effort) = &self.info.settings.reasoning_effort {
            model = format!("{model} · {effort}");
        }
        let mark = "▚▞ bart";
        let version = concat!(" ", env!("CARGO_PKG_VERSION"));
        let mut left = vec![
            Span::styled("▚▞ ", Style::new().fg(brand()).bold()),
            Span::raw("bart").bold(),
        ];
        if Line::raw(mark).width() + 1 + version.len() + Line::raw(&model).width() <= width {
            left.push(Span::styled(version, Style::new().bold().dim()));
        }
        let left = Line::from(left);
        let right = Rect {
            x: area.x + (left.width() as u16 + 1).min(area.width),
            width: area.width.saturating_sub(left.width() as u16 + 1),
            ..area
        };
        frame.render_widget(Paragraph::new(left), area);
        frame.render_widget(
            Paragraph::new(clip_end(&model, usize::from(right.width))).right_aligned(),
            right,
        );
    }

    fn status_left(&self, running_jobs: usize, width: usize, pending: usize) -> Line<'static> {
        let mut items = Vec::new();
        if self.info.recording_stopped {
            items.push(Item {
                full: "REC OFF".into(),
                short: Some(if width >= 4 { "!REC" } else { "!" }.into()),
                priority: 100,
            });
        }
        if self.scrollback.in_history() {
            items.push(Item {
                full: "history".into(),
                short: None,
                priority: 90,
            });
        }
        let marker = if self.turn.is_some() {
            SPIN[self.animation().tick % SPIN.len()]
        } else if running_jobs == 0 {
            "●"
        } else {
            ""
        };
        let activity = if let Some(turn) = &self.turn {
            turn.progress.status(turn.started.elapsed())
        } else if running_jobs > 0 {
            "wait".into()
        } else {
            "ready".into()
        };
        items.push(Item {
            full: format!("{marker} {activity}").trim_start().to_owned(),
            short: self
                .turn
                .as_ref()
                .map(|turn| format!("{marker} {}", turn.progress.compact())),
            priority: 80,
        });
        if running_jobs > 0 {
            items.push(Item {
                full: if running_jobs == 1 {
                    "1 job".to_owned()
                } else {
                    format!("{running_jobs} jobs")
                },
                short: None,
                priority: 70,
            });
        }
        if pending > 0 {
            items.push(Item {
                full: format!("{pending} pending"),
                short: Some(format!("{pending} pend")),
                priority: 60,
            });
        }
        let left_room = if width >= 40 { width - 20 } else { width };
        let text = fit(items, left_room);
        Line::from(
            text.split(" · ")
                .enumerate()
                .flat_map(|(i, part)| {
                    let mut spans = Vec::new();
                    if i > 0 {
                        spans.push(Span::raw(" · "));
                    }
                    if !marker.is_empty()
                        && let Some(rest) = part.strip_prefix(marker)
                    {
                        spans.push(Span::styled(marker, Style::new().fg(brand())));
                        spans.push(Span::styled(rest.to_owned(), Style::new().bold()));
                    } else {
                        let style = if part.contains("REC") {
                            Style::new().fg(theme().error).bold()
                        } else {
                            Style::new().bold()
                        };
                        spans.push(Span::styled(part.to_owned(), style));
                    }
                    spans
                })
                .collect::<Vec<_>>(),
        )
    }

    fn hint_line(&self, width: usize, context: HintContext) -> Line<'static> {
        let interrupt = if width < 8 {
            "Esc"
        } else if width < 30 {
            "Esc stop"
        } else {
            "Esc interrupt"
        };
        let units: &[&str] = match context {
            HintContext::Mention => &["Esc dismiss", "Enter/Tab complete"],
            HintContext::SlashMention => &["Esc dismiss", "Enter command", "Tab complete"],
            HintContext::Slash if self.turn.is_some() && width < 40 => {
                &[interrupt, "Tab run", "Enter run", "↑/↓ select"]
            }
            HintContext::Slash if self.turn.is_some() => {
                &[interrupt, "Enter/Tab run command", "↑/↓ select"]
            }
            HintContext::Slash if width < 24 => &["Tab run", "Enter run", "↑/↓ select"],
            HintContext::Slash => &["Enter/Tab run command", "↑/↓ select"],
            HintContext::Command if self.turn.is_some() => {
                &[interrupt, "Enter command", "Tab queue", "Ctrl+O details"]
            }
            HintContext::Command => &["Enter command", "Ctrl+O details"],
            HintContext::Normal if self.drafts.composer.is_empty() && self.drafts.has_pending() => {
                if self.turn.is_some() {
                    &[interrupt, "↑ edit pending", "Ctrl+O details"]
                } else {
                    &["↑ edit pending", "/ commands", "Ctrl+O details"]
                }
            }
            HintContext::Normal if self.turn.is_some() => {
                &[interrupt, "Enter steer", "Tab queue", "Ctrl+O details"]
            }
            HintContext::Normal => &[
                "/ commands",
                "Ctrl+O details",
                "PgUp/PgDn history",
                "Ctrl+End latest",
            ],
        };
        Line::from(fit(
            units.iter().enumerate().map(|(i, full)| Item {
                full: (*full).to_owned(),
                short: None,
                priority: usize::MAX - i,
            }),
            width,
        ))
    }

    fn composer_rows(&mut self, width: u16) -> usize {
        let room = usize::from(width.saturating_sub(3)).max(1);
        let (rows, cursor) = self.drafts.composer.layout(room);
        rows.len().max(cursor.0 + 1).min(MAX_COMPOSER_ROWS)
    }

    fn pending_lines(&self, width: u16, rows: u16) -> Vec<Line<'static>> {
        if rows == 0 || width == 0 {
            return Vec::new();
        }
        let count = self.drafts.steers.len() + self.drafts.queued.len();
        let shown = usize::from(rows).saturating_sub(usize::from(count > usize::from(rows)));
        let mut lines: Vec<_> = self
            .drafts
            .steers
            .iter()
            .map(|pending| ("↳ ", "steering", &pending.display))
            .chain(
                self.drafts
                    .queued
                    .iter()
                    .map(|pending| ("⋯ ", "queued", &pending.display)),
            )
            .take(shown)
            .map(|(marker, state, text)| {
                let prefix: String = text.chars().take(usize::from(width)).collect();
                let more = prefix.len() < text.len();
                Line::from(vec![
                    Span::styled(marker, Style::new().fg(brand())),
                    Span::raw(clip_end(
                        &format!(
                            "{state} · {}{}",
                            prefix.replace('\n', " ⏎ "),
                            if more { " " } else { "" }
                        ),
                        usize::from(width.saturating_sub(2)),
                    )),
                ])
            })
            .collect();
        if shown < count {
            lines.push(Line::from(clip_end(
                &format!("  {} more pending messages", count - shown),
                usize::from(width),
            )));
        }
        lines
    }

    fn draw_live(&mut self, frame: &mut Frame, area: Rect) -> HintContext {
        let mut lines = Vec::new();
        let (menu, context) = if matches!(&self.surface, Some(Surface::Approval(answer)) if answer.answer.pending())
        {
            (Vec::new(), HintContext::Normal)
        } else {
            self.menu_rows()
        };
        let selecting = !menu.is_empty();
        let area = if selecting {
            let menu_height = u16::try_from(menu.len() + 2)
                .unwrap_or(u16::MAX)
                .min(area.height)
                .min(10);
            let [conversation, menu_area] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(menu_height)])
                    .areas(area);
            self.geometry.conversation = conversation;
            let history = self
                .scrollback
                .view(usize::from(conversation.height), &self.transcript);
            frame.render_widget(Paragraph::new(history), conversation);
            lines = Vec::new();
            let block = ratatui::widgets::Block::bordered()
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::new().dim())
                .padding(ratatui::widgets::Padding::horizontal(1));
            let inner = block.inner(menu_area);
            frame.render_widget(block, menu_area);
            inner
        } else {
            area
        };
        let selected = self.drafts.menu.min(menu.len().saturating_sub(1));
        let first = selected.saturating_sub(usize::from(area.height).saturating_sub(1));
        for (index, mut row) in menu
            .into_iter()
            .enumerate()
            .skip(first)
            .take(usize::from(area.height))
        {
            let marker = if index == selected {
                Span::styled("❯ ", Style::new().fg(brand()).bold())
            } else {
                Span::raw("  ")
            };
            row.insert(0, marker);
            lines.push(super::super::markdown::RenderRow::Literal(Line::from(row)));
        }
        let lines = if !selecting {
            self.geometry.conversation = area;
            self.scrollback
                .view(usize::from(area.height), &self.transcript)
        } else {
            lines
                .into_iter()
                .flat_map(|line| line.project(area.width))
                .collect()
        };
        let paragraph = Paragraph::new(lines);
        let height = u16::try_from(paragraph.line_count(area.width)).unwrap_or(u16::MAX);
        let scroll = height.saturating_sub(area.height);
        frame.render_widget(paragraph.scroll((scroll, 0)), area);

        context
    }

    fn menu_rows(&self) -> (Vec<Vec<Span<'static>>>, HintContext) {
        let found = self.mentions();
        if !found.is_empty() {
            return (
                found
                    .into_iter()
                    .map(|found| {
                        found
                            .display
                            .chars()
                            .enumerate()
                            .map(|(at, c)| {
                                let span = Span::raw(c.to_string());
                                if found.matched.contains(&at) {
                                    span.fg(brand()).bold()
                                } else {
                                    span
                                }
                            })
                            .collect()
                    })
                    .collect(),
                if parse_slash(self.drafts.composer.text()).is_some() {
                    HintContext::SlashMention
                } else {
                    HintContext::Mention
                },
            );
        }
        let menu = self.menu();
        let context = if !menu.is_empty() {
            HintContext::Slash
        } else if parse_slash(self.drafts.composer.text()).is_some() {
            HintContext::Command
        } else {
            HintContext::Normal
        };
        (
            menu.into_iter()
                .map(|(_, name, about)| {
                    vec![Span::raw(format!("/{name:<8} ")).bold(), Span::raw(about)]
                })
                .collect(),
            context,
        )
    }

    fn usage_line(&self, room: usize) -> Line<'static> {
        let usage = fit(
            self.usage().iter().enumerate().map(|(i, full)| Item {
                full: full.clone(),
                short: None,
                priority: usize::MAX - i,
            }),
            room,
        );
        match usage.strip_prefix(self.info.confinement.label()) {
            Some(rest) => Line::from(vec![
                Span::raw(self.info.confinement.label()).bold(),
                Span::raw(rest.to_owned()),
            ]),
            None => Line::from(usage),
        }
    }

    fn usage(&self) -> Vec<String> {
        let thousands = |tokens: u64| format!("{}.{:01}k", tokens / 1000, (tokens % 1000) / 100);
        let mut parts = vec![self.info.confinement.label().to_owned()];
        let accounting = &self.info.accounting;
        if accounting.context_tokens > 0 {
            parts.push(format!("~{} context", thousands(accounting.context_tokens)));
        }
        let usage = &accounting.usage;
        if let Some(last) = usage.last
            && last.input_tokens > 0
            && let Some(cached) = last.cache_read_tokens
        {
            let percent =
                u128::from(cached.min(last.input_tokens)) * 100 / u128::from(last.input_tokens);
            parts.push(format!("{percent}% last cached"));
        }
        if let Some(output) = usage.output_tokens {
            let mut reports = format!("{} out", thousands(output));
            if let Some(cost) = usage.cost {
                let _ = write!(
                    reports,
                    ", {}${cost:.3}{}",
                    if usage.cost_is_estimate { "~" } else { "" },
                    if usage.cost_is_estimate {
                        " reported"
                    } else {
                        ""
                    }
                );
            }
            parts.push(reports);
        }
        for window in self
            .info
            .limits
            .iter()
            .flat_map(|limits| [limits.short, limits.long])
            .flatten()
            .filter(|window| window.minutes != Some(0))
        {
            let span = match window.minutes {
                Some(10_080) => "week".to_owned(),
                Some(minutes) if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
                Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
                Some(minutes) => format!("{minutes}m"),
                None => "limit".to_owned(),
            };
            parts.push(format!("{span} {:.0}% used", window.used_percent));
        }
        parts
    }
}

mod composer;
use composer::draw_composer;
