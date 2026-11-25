use crate::tui::text::clip_end;
use crate::tui::text::{Item, fit, width as text_width};

use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{App, MAX_COMPOSER_ROWS, Surface, parse_slash};
use crate::tui::SPIN;
use crate::tui::{brand, theme};

const STATUS_SEPARATOR: &str = " · ";
const WIDE_STATUS_WIDTH: usize = 100;

#[derive(Clone, Copy)]
enum HintContext {
    Mention,
    SlashMention,
    Slash,
    Command,
    Normal,
}

pub(in crate::tui) use crate::tui::widgets::confirm::FrameStamp;

type TerminalSizeProbe = fn() -> std::io::Result<(u16, u16)>;

pub(in crate::tui) struct Painted {
    pub(super) stamp: FrameStamp,
    pub(super) review: bool,
    pub(super) conversation: bool,
    pub(super) terminal_size: Option<TerminalSizeProbe>,
    pub(super) spinner_visible: bool,
}

impl App {
    pub(super) fn render_frame<B: Backend>(
        &mut self,
        screen: &mut Terminal<B>,
        running_jobs: usize,
    ) -> Result<Painted, B::Error> {
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
            crate::tui::links::paint(frame.buffer_mut());
        })?;
        let stamp = FrameStamp {
            seq: self.paint.seq + 1,
            size: (completed.area.width, completed.area.height),
        };
        let review = reviewable;
        Ok(Painted {
            stamp,
            review,
            conversation: self.surface.is_none(),
            terminal_size: None,
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
                return confirm.draw(
                    frame,
                    area,
                    feedback,
                    confirm.layout(area, feedback.is_some()),
                );
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
            let layout = answer.confirm.layout(area, feedback.is_some());
            if let Some(panel) = layout.panel {
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
                layout,
            );
        }
        let lead = 2;
        let shown = self.composer_rows(frame.area().width);
        let find_mode = self.surface.is_none() && self.scrollback.search().in_mode();
        let transcript_rows = u16::from(find_mode);
        let hint_rows = u16::from(
            find_mode
                || self.turn.is_some()
                || self.status.text().is_some()
                || usize::from(frame.area().height) >= shown + 6,
        );
        let top_rows = frame
            .area()
            .height
            .saturating_sub(4 * transcript_rows)
            .min(2);
        let height = frame.area().height - top_rows;
        if top_rows > 0 {
            self.draw_top_bar(frame);
        }
        let shown = u16::try_from(shown).unwrap_or(1);
        let decorated = height >= shown.saturating_add(4 + hint_rows + transcript_rows);
        let status_rows = u16::from(height > 1 + 2 * transcript_rows);
        let hint_rows = hint_rows.min(height.saturating_sub(status_rows + 1 + transcript_rows));
        let draft_rows = shown
            .min(height.saturating_sub(status_rows + hint_rows + transcript_rows))
            .max(u16::from(height > 0));
        let borders = u16::from(decorated) * 2;
        let spacer = u16::from(decorated);
        let available =
            height.saturating_sub(draft_rows + borders + spacer + status_rows + hint_rows);
        let pending_rows =
            u16::try_from((self.drafts.steers.len() + self.drafts.queued.len()).min(3))
                .expect("at most three rows")
                .min(available.saturating_sub(transcript_rows));
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
        draw_composer(
            frame,
            composer,
            &mut self.drafts.composer,
            lead,
            decorated,
            self.info.name.as_deref(),
            !find_mode,
        );

        let count = self.drafts.steers.len() + self.drafts.queued.len();
        let pending = if pending_rows == 0 { count } else { 0 };
        if let Some(note) = find_mode.then(|| self.status_hint(status.width)).flatten() {
            frame.render_widget(note, status);
        } else {
            self.draw_status(frame, status, running_jobs, pending);
        }
        if hint_rows > 0 {
            let find_bar = find_mode
                .then(|| {
                    self.scrollback
                        .search()
                        .prompt_line(hints.width, "Enter find")
                        .or_else(|| self.scrollback.search().nav_line(hints.width))
                })
                .flatten();
            let hint = find_bar
                .or_else(|| self.status_hint(hints.width))
                .unwrap_or_else(|| self.hint_line(usize::from(hints.width), hint_context));
            frame.render_widget(hint, hints);
        }
        false
    }

    fn status_hint(&self, width: u16) -> Option<Line<'static>> {
        self.status.text().map(|text| {
            let text = format!("  {text}");
            Line::from(crate::tui::links::clipped(
                &text,
                clip_end(&text, usize::from(width)),
                Style::new().fg(brand()),
            ))
        })
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

    fn draw_status(&self, frame: &mut Frame, area: Rect, running_jobs: usize, pending: usize) {
        let wide = usize::from(area.width) >= WIDE_STATUS_WIDTH;
        let prefix = self.usage_prefix(wide);
        let reserved = joined_status_width(prefix.iter().flatten().map(Span::width));
        let left = self.status_left(running_jobs, usize::from(area.width), pending, reserved);
        let left_width = u16::try_from(left.width())
            .unwrap_or(area.width)
            .min(area.width);
        let gap = u16::from(left_width > 0 && left_width < area.width);
        let right_start = left_width + gap;
        let right_width = area.width - right_start;
        frame.render_widget(
            Paragraph::new(left),
            Rect {
                width: left_width,
                ..area
            },
        );
        let right = if wide {
            usage_line(
                prefix.into_iter().flatten().chain(self.usage_details()),
                usize::from(right_width),
            )
        } else {
            usage_line(prefix.into_iter().flatten(), usize::from(right_width))
        };
        frame.render_widget(
            Paragraph::new(right.right_aligned()),
            Rect {
                x: area.x.saturating_add(right_start),
                width: right_width,
                ..area
            },
        );
    }

    fn status_left(
        &self,
        running_jobs: usize,
        width: usize,
        pending: usize,
        reserved: usize,
    ) -> Line<'static> {
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
        let activity = Item {
            full: format!("{marker} {activity}").trim_start().to_owned(),
            short: self
                .turn
                .as_ref()
                .map(|turn| format!("{marker} {}", turn.progress.compact())),
            priority: 80,
        };
        let minimum = joined_status_width(items.iter().map(|item| text_width(&item.full)).chain(
            std::iter::once(text_width(
                activity.short.as_deref().unwrap_or(&activity.full),
            )),
        ));
        items.push(activity);
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
        let left_room = width.saturating_sub(reserved + 1).max(minimum.min(width));
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
        let main = self.surface.is_none();
        let empty = self.drafts.composer.is_empty();
        let slash = if main && empty && self.scrollback.in_history() {
            "/ search"
        } else {
            "/ commands"
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
                    &["↑ edit pending", slash, "Ctrl+O details"]
                }
            }
            HintContext::Normal if self.turn.is_some() => {
                &[interrupt, "Enter steer", "Tab queue", "Ctrl+O details"]
            }
            HintContext::Normal => &[
                slash,
                "Ctrl+O details",
                "PgUp/PgDn history",
                "Ctrl+End latest",
            ],
        };
        Line::from(fit(
            units
                .iter()
                .copied()
                .chain(main.then_some("Ctrl+F search"))
                .enumerate()
                .map(|(i, full)| Item {
                    full: full.to_owned(),
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
        let find_mode = self.surface.is_none() && self.scrollback.search().in_mode();
        let (menu, context) = if find_mode
            || matches!(&self.surface, Some(Surface::Approval(answer)) if answer.answer.pending())
        {
            (Vec::new(), HintContext::Normal)
        } else {
            self.menu_rows()
        };
        let transcript_rows = u16::from(find_mode);
        let menu_height = u16::try_from(menu.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(transcript_rows))
            .min(10);
        let (conversation, menu_area) = if menu.is_empty() || menu_height == 0 {
            (area, None)
        } else {
            let [conversation, menu_area] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(menu_height)])
                    .areas(area);
            (conversation, Some(menu_area))
        };
        self.geometry.conversation = conversation;
        let history = self
            .scrollback
            .view(usize::from(conversation.height), &self.transcript);
        let buf = frame.buffer_mut();
        for (index, line) in history
            .iter()
            .take(usize::from(conversation.height))
            .enumerate()
        {
            crate::tui::text::render_line(
                line,
                Rect::new(
                    conversation.x,
                    conversation.y + index as u16,
                    conversation.width,
                    1,
                ),
                buf,
            );
        }
        let Some(menu_area) = menu_area else {
            return context;
        };
        let block = ratatui::widgets::Block::bordered()
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::new().dim())
            .padding(ratatui::widgets::Padding::horizontal(1));
        let area = block.inner(menu_area);
        frame.render_widget(block, menu_area);
        let selected = self.drafts.menu.min(menu.len().saturating_sub(1));
        let first = selected.saturating_sub(usize::from(area.height).saturating_sub(1));
        let lines: Vec<_> = menu
            .into_iter()
            .enumerate()
            .skip(first)
            .take(usize::from(area.height))
            .map(|(index, mut row)| {
                let marker = if index == selected {
                    Span::styled("❯ ", Style::new().fg(brand()).bold())
                } else {
                    Span::raw("  ")
                };
                row.insert(0, marker);
                Line::from(row)
            })
            .collect();
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
                        let style = crate::tui::links::style(
                            Style::new(),
                            crate::tui::links::file(&self.shared.opener.cwd.join(&found.path)),
                        );
                        found
                            .display
                            .chars()
                            .enumerate()
                            .map(|(at, c)| {
                                let span = Span::styled(c.to_string(), style);
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

    fn usage_prefix(&self, wide: bool) -> [Option<Span<'static>>; 3] {
        let accounting = &self.info.accounting;
        let context = (accounting.context_tokens > 0).then(|| {
            let Some(budget) = accounting.context_budget else {
                return Span::raw(format!("~{}", compact_tokens(accounting.context_tokens)));
            };
            let used = u128::from(accounting.context_tokens) * 100;
            let threshold = u128::from(budget.compaction_threshold());
            let style = if used >= threshold * 95 {
                Style::new().fg(theme().error).bold()
            } else if used >= threshold * 80 {
                Style::new().fg(theme().literal).bold()
            } else if wide {
                Style::new()
            } else {
                Style::new().fg(theme().success)
            };
            let label = if wide {
                format!(
                    "{}/{}",
                    compact_tokens(accounting.context_tokens),
                    compact_tokens(budget.window())
                )
            } else {
                let window = u128::from(budget.window());
                let percent = (used + window / 2) / window;
                format!("{percent}%")
            };
            Span::styled(label, style)
        });
        let usage = &accounting.usage;
        let cost = usage
            .cost
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map(|cost| Span::raw(format_cost(cost.abs(), usage.cost_is_estimate)));
        [
            Some(Span::raw(self.info.confinement.label()).bold()),
            context,
            cost,
        ]
    }

    fn usage_details(&self) -> impl Iterator<Item = Span<'static>> + '_ {
        let usage = &self.info.accounting.usage;
        let cache = usage.last.and_then(|last| {
            let cached = u128::from(last.cache_read_tokens?.min(last.input_tokens));
            let percent = (cached * 100).checked_div(u128::from(last.input_tokens))?;
            Some(Span::raw(format!("{percent}% cached")))
        });
        let output = usage
            .output_tokens
            .map(|tokens| Span::raw(format!("{} out", compact_tokens(tokens))));
        let limits = self
            .info
            .limits
            .iter()
            .flat_map(|limits| [limits.short, limits.long])
            .flatten()
            .filter(|window| window.minutes != Some(0))
            .map(|window| {
                let span = match window.minutes {
                    Some(10_080) => "week".to_owned(),
                    Some(minutes) if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
                    Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
                    Some(minutes) => format!("{minutes}m"),
                    None => "limit".to_owned(),
                };
                Span::raw(format!("{span} {:.0}% used", window.used_percent))
            });
        cache.into_iter().chain(output).chain(limits)
    }
}

fn joined_status_width(widths: impl Iterator<Item = usize>) -> usize {
    let separator = text_width(STATUS_SEPARATOR);
    widths
        .reduce(|total, width| total + separator + width)
        .unwrap_or(0)
}

fn usage_line(items: impl Iterator<Item = Span<'static>>, room: usize) -> Line<'static> {
    let separator = Span::raw(STATUS_SEPARATOR);
    let mut spans = Vec::new();
    let mut remaining = room;
    for item in items {
        let gap = if spans.is_empty() {
            0
        } else {
            separator.width()
        };
        let Some(rest) = remaining.checked_sub(gap + item.width()) else {
            break;
        };
        remaining = rest;
        if gap > 0 {
            spans.push(separator.clone());
        }
        spans.push(item);
    }
    Line::from(spans)
}

fn compact_tokens(tokens: u64) -> String {
    fn tenths(value: u64, suffix: char) -> String {
        let whole = value / 10;
        let tenth = value % 10;
        if tenth == 0 {
            format!("{whole}{suffix}")
        } else {
            format!("{whole}.{tenth}{suffix}")
        }
    }

    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..10_000 => tenths((tokens + 50) / 100, 'k'),
        10_000..999_500 => format!("{}k", (tokens + 500) / 1_000),
        _ => tenths(
            tokens / 100_000 + u64::from(tokens % 100_000 >= 50_000),
            'M',
        ),
    }
}

fn format_cost(cost: f64, estimate: bool) -> String {
    let prefix = if estimate { "~" } else { "" };
    if cost > 0.0 && cost < 0.0005 {
        format!("{prefix}<$0.001")
    } else {
        let decimals = if cost > 0.0 && cost < 0.01 { 3 } else { 2 };
        format!("{prefix}${cost:.decimals$}")
    }
}

mod composer;
use composer::draw_composer;

#[cfg(test)]
mod tests;
