use super::{Call, DisplayDiff, Finished, muted};
use crate::tui::text::{self, Clean};
use crate::{
    tool::Status,
    tui::{text::clip_end, theme},
};
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use std::{borrow::Cow, time::Duration};

const MAX_DIFF_ROWS: usize = 40;
use crate::tui::SPIN;

pub(super) struct Subject {
    pub(super) name: Clean,
    pub(super) rest: Clean,
}

const HELPERS: [&str; 8] = [
    "run",
    "bg_run",
    "read",
    "write",
    "edit",
    "mcp",
    "web_search",
    "display",
];

impl Subject {
    pub(super) fn of(tool: &str, code: &str) -> Self {
        let mut lines = code.lines().map(str::trim).filter(|line| !line.is_empty());
        let first = lines.next().unwrap_or_default();
        let first = &first[..first.floor_char_boundary(first.len().min(500))];
        let (name, rest) = if lines.next().is_some() {
            (String::new(), format!("cell · {first} …"))
        } else if let Some((helper, arguments)) = helper_call(first) {
            (helper.to_owned(), arguments.to_owned())
        } else {
            (String::new(), first.to_owned())
        };
        if tool == "python" {
            Self {
                name: Clean::new(&name),
                rest: Clean::new(&rest),
            }
        } else {
            Self {
                name: Clean::new(tool),
                rest: Clean::new(&format!(" {name}{rest}")),
            }
        }
    }
}

fn helper_call(line: &str) -> Option<(&'static str, &str)> {
    if line.contains("'''") || line.contains(r#"""""#) {
        return None;
    }
    let inner =
        if line.starts_with("print(") && closing(line, "print".len()) == Some(line.len() - 1) {
            &line["print(".len()..line.len() - 1]
        } else {
            line
        };
    let helper = HELPERS.into_iter().find(|helper| {
        inner
            .strip_prefix(helper)
            .is_some_and(|after| after.starts_with('('))
    })?;
    (closing(inner, helper.len()) == Some(inner.len() - 1))
        .then(|| (helper, &inner[helper.len()..]))
}

fn closing(code: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (at, c) in code.char_indices().skip_while(|&(at, _)| at < open) {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(_) if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '#' => return None,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

fn summary(output: &str, status: Status, jobs: &[u32]) -> (String, Vec<String>) {
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| {
            !line.trim().is_empty()
                && !line.starts_with(crate::marker::PREFIX)
                && !line.starts_with("{\"process")
        })
        .collect();
    let mut summary = match status {
        Status::Failed => lines
            .last()
            .copied()
            .or_else(|| {
                let line = output
                    .lines()
                    .rfind(|line| line.starts_with(crate::marker::PREFIX))?;
                line.strip_prefix(crate::marker::PREFIX)?.strip_suffix(']')
            })
            .unwrap_or("failed")
            .to_owned(),
        Status::Succeeded => count(output.lines().count()),
    };
    if !jobs.is_empty() {
        let ids: Vec<String> = jobs.iter().map(u32::to_string).collect();
        summary = format!("{summary} · started job {}", ids.join(", "));
    }
    let expand = if status == Status::Failed {
        lines[lines.len().saturating_sub(3)..lines.len().saturating_sub(1)]
            .iter()
            .map(|line| (*line).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    (summary, expand)
}

fn count(lines: usize) -> String {
    if lines == 1 {
        "1 line".into()
    } else {
        format!("{lines} lines")
    }
}

const RUNNING_SHOWN: usize = 3;

fn live_activities(
    call: &Call,
    live: Option<&crate::tool::Live>,
) -> Option<crate::tool::Activities> {
    call.live
        .zip(live)
        .and_then(|(id, live)| live.activities(id))
}

pub(super) fn tools(
    calls: &[Call],
    live: Option<&crate::tool::Live>,
    width: u16,
    tick: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for call in calls {
        let theme = theme();
        let (glyph, color, summary, expand, elapsed) = match &call.result {
            None => (
                SPIN[tick % SPIN.len()],
                theme.brand,
                "running…".to_owned(),
                Vec::new(),
                Some(Duration::from_secs(call.started.elapsed().as_secs())),
            ),
            Some(Finished {
                output,
                status,
                elapsed,
            }) => {
                let (glyph, color) = match status {
                    Status::Succeeded => ("✓", theme.success),
                    Status::Failed => ("✗", theme.error),
                };
                let (summary, expand) = summary(output, *status, &call.jobs);
                (glyph, color, summary, expand, *elapsed)
            }
        };
        let failed = matches!(
            call.result,
            Some(Finished {
                status: Status::Failed,
                ..
            })
        );
        let duration = elapsed.map_or_else(String::new, |elapsed| {
            format!("  {:.1}s", elapsed.as_secs_f64())
        });
        let width = usize::from(width);
        let summary = clip_end(&summary, width / 2);
        let right = format!("{summary}{duration}");
        let Subject { name, rest } = &call.subject;
        let room = width.saturating_sub(text::width(&right) + 5 + text::width(name));
        let rest = clip_end(rest, room);
        let left = format!("{glyph} {name}{rest}");
        let gap = width
            .saturating_sub(text::width(&left) + text::width(&right) + 1)
            .max(2);
        let quiet = muted(Style::new().italic());
        let summary_style = if failed {
            Style::new().fg(theme.error)
        } else {
            quiet
        };
        lines.push(Line::from(vec![
            Span::styled(glyph, Style::new().fg(color)),
            Span::raw(" "),
            Span::styled(name.clone(), quiet.bold()),
            Span::styled(rest, quiet),
            Span::raw(" ".repeat(gap)),
            Span::styled(summary, summary_style),
            Span::styled(duration, quiet),
        ]));
        for line in expand {
            lines.push(Line::from(vec![
                Span::raw("    ").dim(),
                Span::styled(line, Style::new().fg(theme.error)),
            ]));
        }
        if call.result.is_none()
            && let Some(activities) = live_activities(call, live)
        {
            let running: Vec<_> = activities
                .rows
                .into_iter()
                .filter(|activity| activity.state == crate::tool::LiveState::Running)
                .collect();
            for activity in running.iter().take(RUNNING_SHOWN) {
                lines.push(Line::styled(
                    clip_end(&format!("  ↳ {}", activity.title), width),
                    quiet,
                ));
            }
            if running.len() > RUNNING_SHOWN {
                lines.push(Line::styled(
                    clip_end(
                        &format!("  ↳ +{} more running", running.len() - RUNNING_SHOWN),
                        width,
                    ),
                    quiet,
                ));
            }
        }
        lines.extend(diff_rows(&call.diffs, "    "));
    }
    lines
}

pub(super) fn activities(
    calls: &[Call],
    live: Option<&crate::tool::Live>,
    width: u16,
    tick: usize,
) -> Vec<Line<'static>> {
    use crate::tool::LiveState;
    let mut lines = Vec::new();
    for call in calls {
        let recorded = match &call.result {
            Some(_) => call.activities.as_ref().map(Cow::Borrowed),
            None => live_activities(call, live).map(Cow::Owned),
        };
        let recorded = recorded.unwrap_or_default();
        let omitted = recorded.omitted;
        for activity in &recorded.rows {
            let (glyph, color) = match activity.state {
                LiveState::Running => (SPIN[tick % SPIN.len()], theme().brand),
                LiveState::Finished => ("✓", theme().success),
                LiveState::Failed => ("✗", theme().error),
                LiveState::Interrupted => ("!", theme().error),
            };
            let suffix = match activity.state {
                LiveState::Running => "running…",
                LiveState::Finished => "",
                LiveState::Failed => "failed",
                LiveState::Interrupted => "interrupted · effects unknown",
            };
            let room = usize::from(width).saturating_sub(text::width(suffix) + 5);
            let title = clip_end(&activity.title, room);
            let gap = usize::from(width)
                .saturating_sub(text::width(&title) + text::width(suffix) + 3)
                .max(2);
            let (function, arguments) = title
                .split_once('(')
                .map_or((title.as_str(), None), |(name, rest)| (name, Some(rest)));
            let mut spans = vec![
                Span::styled(glyph, Style::new().fg(color)),
                Span::raw(" "),
                Span::styled(function.to_owned(), Style::new().fg(theme().brand).bold()),
            ];
            if let Some(arguments) = arguments {
                spans.push(Span::styled("(", muted(Style::new())));
                if let Some(arguments) = arguments.strip_suffix(')') {
                    spans.push(Span::raw(arguments.to_owned()));
                    spans.push(Span::styled(")", muted(Style::new())));
                } else {
                    spans.push(Span::raw(arguments.to_owned()));
                }
            }
            spans.extend([
                Span::raw(" ".repeat(gap)),
                Span::styled(suffix, Style::new().fg(color)),
            ]);
            lines.push(Line::from(spans));
        }
        if omitted > 0 {
            lines.push(Line::styled(
                format!("  {omitted} additional activities omitted"),
                muted(Style::new()),
            ));
        }
        // Python execution itself can fail outside any builtin, or between two successful ones.
        if let Some(result) = &call.result
            && result.status == Status::Failed
        {
            lines.push(Line::styled(
                clip_end(
                    "✗ Python cell failed or interrupted · Ctrl+O details",
                    usize::from(width),
                ),
                Style::new().fg(theme().error),
            ));
        }
        lines.extend(diff_rows(&call.diffs, "    "));
    }
    lines
}

pub(super) fn diff_rows(diffs: &[DisplayDiff], prefix: &str) -> Vec<Line<'static>> {
    let quiet = muted(Style::new());
    let rows = diffs.iter().flat_map(|diff| {
        std::iter::once((quiet.bold(), &*diff.path, false)).chain(diff.hunks.lines().map(|line| {
            let (style, bar) = diff_style(line);
            (style, line, bar)
        }))
    });
    let total = diffs
        .iter()
        .map(|diff| 1 + diff.hunks.lines().count())
        .sum::<usize>();
    let mut lines: Vec<Line<'static>> = rows
        .take(if total > MAX_DIFF_ROWS {
            MAX_DIFF_ROWS - 1
        } else {
            total
        })
        .map(|(style, text, bar)| {
            let background = if bar { style.bg } else { None };
            Line::from(vec![
                Span::styled(prefix.to_owned(), Style::new().dim().bg(Color::Reset)),
                Span::styled(text.to_owned(), style),
            ])
            .style(Style {
                bg: background,
                ..Style::new()
            })
        })
        .collect();
    if total > MAX_DIFF_ROWS {
        lines.push(Line::from(vec![
            Span::raw(prefix.to_owned()).dim(),
            Span::styled(
                format!(
                    "… {} more diff lines; Ctrl+O shows them",
                    total - MAX_DIFF_ROWS + 1
                ),
                quiet,
            ),
        ]));
    }
    lines
}

pub(super) fn diff_style(line: &str) -> (Style, bool) {
    let theme = theme();
    let bar = |wash: Option<Color>, text: Color| {
        wash.map_or_else(|| Style::new().fg(text), |wash| Style::new().bg(wash))
    };
    match line.as_bytes().first() {
        Some(b'-') => (bar(theme.removed, theme.error), true),
        Some(b'+') => (bar(theme.added, theme.success), true),
        Some(b'@' | b'\\') => (muted(Style::new()), false),
        _ => (Style::new(), false),
    }
}
