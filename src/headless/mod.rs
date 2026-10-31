use std::io::{self, BufRead as _, IsTerminal as _, Read as _, Write as _};

use tokio::sync::{mpsc, watch};

use crate::{
    agent::{self, Event, Session},
    history::{Notification, ToolCall},
    provider::Delta,
    signals::Terminations,
    terminal_text,
};

pub(crate) async fn run(
    mut session: Session,
    prompt: Option<String>,
    mut terminations: Terminations,
) -> Result<(), String> {
    let registry = session.tool_context().mcp();
    let result = {
        let running = headless_session(&mut session, prompt, &mut terminations);
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => result,
            () = registry.report_failures() => running.await,
        }
    };
    if let Some(reason) = session.finish().await {
        terminal_text::stderr_line(format_args!("bart: {reason}"));
    }
    result
}

async fn headless_session(
    session: &mut Session,
    prompt: Option<String>,
    terminations: &mut Terminations,
) -> Result<(), String> {
    session.prewarm().await;
    let output = Output::default();
    let mut display = HeadlessDisplay::new(&output);
    let mut lines = None;
    let mut failed = 0_usize;
    match prompt {
        Some(prompt) => {
            if !cancellable(
                session.turn(prompt.into(), |event: crate::agent::Event<'_>| {
                    display.event(event);
                }),
                terminations,
                &output,
            )
            .await?
            {
                return Ok(());
            }
        }
        None => {
            lines = Some(stdin_lines()?);
        }
    }
    loop {
        display = HeadlessDisplay::new(&output);
        if lines.is_none() && !session.has_pending_commands() {
            return match failed {
                0 => Ok(()),
                1 => Err("a turn failed".into()),
                n => Err(format!("{n} turns failed")),
            };
        }
        let result = tokio::select! {
            line = async { lines.as_mut().expect("guarded").recv().await.transpose() }, if lines.is_some() => match line? {
                None => {
                    lines = None;
                    continue;
                }
                Some(line) if line.trim().is_empty() => continue,
                Some(line) if line.trim() == "/compact" => cancellable(session.compact(|event: crate::agent::Event<'_>| display.event(event)), terminations, &output).await,
                Some(line) => cancellable(session.turn(line.into(), |event: crate::agent::Event<'_>| display.event(event)), terminations, &output).await,
            },
            () = session.jobs_ready(), if session.has_jobs() => {
                cancellable(session.wake(|event: crate::agent::Event<'_>| display.event(event)), terminations, &output).await
            }
            _ = terminations.requested() => return Ok(()),
        };
        match result {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(error) if output.failure().is_some() => return Err(error),
            Err(error) => {
                crate::terminal_text::stderr_line(format_args!("bart: {error}"));
                failed += 1;
            }
        }
    }
}

fn stdin_lines() -> Result<mpsc::Receiver<Result<String, String>>, String> {
    let (sender, receiver) = mpsc::channel(1);
    // OS stdin reads must not belong to the runtime or be joined at shutdown.
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            loop {
                let result = match stdin_line(&mut input) {
                    Ok(Some(line)) => Ok(line),
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
                let failed = result.is_err();
                if sender.blocking_send(result).is_err() || failed {
                    break;
                }
            }
        })
        .map_err(|error| format!("cannot start stdin reader: {error}"))?;
    Ok(receiver)
}

const MAX_STDIN_LINE_BYTES: usize = 16 * 1024 * 1024;

fn stdin_line(input: &mut impl io::BufRead) -> Result<Option<String>, String> {
    let mut line = Vec::new();
    input
        .take((MAX_STDIN_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("cannot read stdin: {error}"))?;
    if line.len() > MAX_STDIN_LINE_BYTES {
        return Err(format!(
            "a line of stdin exceeds {} MiB",
            MAX_STDIN_LINE_BYTES >> 20
        ));
    }
    if line.is_empty() {
        return Ok(None);
    }
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| "stdin is not UTF-8".to_owned())
}

async fn cancellable(
    turn: impl Future<Output = Result<(), agent::Error>>,
    terminations: &mut Terminations,
    output: &Output,
) -> Result<bool, String> {
    let outcome = tokio::select! {
        result = turn => Some(result),
        _ = terminations.requested() => None,
        error = output.failed() => return Err(error),
    };
    if let Some(error) = output.failure() {
        return Err(error);
    }
    if let Some(result) = outcome {
        return result.map(|()| true).map_err(|error| error.to_string());
    }
    crate::terminal_text::stderr_line(format_args!("[cancelled]"));
    Ok(false)
}

#[derive(Clone, Default)]
struct Output(watch::Sender<Option<String>>);

impl Output {
    fn write(&self, text: &str) {
        if self.failure().is_some() {
            return;
        }
        let mut stdout = io::stdout().lock();
        if let Err(error) = stdout
            .write_all(text.as_bytes())
            .and_then(|()| stdout.flush())
        {
            self.0
                .send_replace(Some(format!("cannot write to stdout: {error}")));
        }
    }

    fn failure(&self) -> Option<String> {
        self.0.borrow().clone()
    }

    async fn failed(&self) -> String {
        let mut failure = self.0.subscribe();
        match failure.wait_for(Option::is_some).await {
            Ok(failure) => failure.clone().expect("waited for a failure"),
            Err(_) => std::future::pending().await,
        }
    }
}

struct HeadlessDisplay {
    text: terminal_text::Display,
    reasoning: terminal_text::Display,
    response: String,
    output: Output,
}

impl HeadlessDisplay {
    fn new(output: &Output) -> Self {
        Self {
            text: terminal_text::Display::new(io::stdout().is_terminal()),
            reasoning: terminal_text::Display::new(io::stderr().is_terminal()),
            response: String::new(),
            output: output.clone(),
        }
    }

    fn publish(&mut self) {
        let mut text = std::mem::take(&mut self.response);
        if text.is_empty() {
            return;
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        self.output.write(&text);
    }

    fn event(&mut self, event: Event<'_>) {
        if matches!(event, Event::Responded(_)) {
            self.publish();
        }
        if matches!(
            event,
            Event::Responded(_) | Event::Retrying { .. } | Event::WaitingForReset { .. }
        ) {
            *self = Self::new(&self.output);
        }
        match event {
            Event::Delta(Delta::Text(text)) => {
                self.text.push(text, |text| self.response.push_str(text));
            }
            Event::Delta(Delta::Reasoning(text)) => self.reasoning.push(text, |text| {
                let _ = io::stderr().write_all(text.as_bytes());
            }),
            Event::ToolStarted { call, .. } => {
                crate::terminal_text::stderr_line(format_args!("\n→ {}", summary(call)));
            }
            Event::ToolFinished {
                call,
                output,
                elapsed,
            } => {
                let lines = output.text.lines().count();
                crate::terminal_text::stderr_line(format_args!(
                    "{} {} ({:.1}s, {lines} line{})",
                    if output.is_error() { "✗" } else { "✓" },
                    call.name,
                    elapsed.as_secs_f64(),
                    if lines == 1 { "" } else { "s" }
                ));
            }
            Event::Review(review) => {
                crate::terminal_text::stderr_line(format_args!("  {}", review.line()));
            }
            Event::Image(image) => {
                crate::terminal_text::stderr_line(format_args!(
                    "  image: {}",
                    image.path.display()
                ));
            }
            Event::SkillAttached(skill) => {
                crate::terminal_text::stderr_line(format_args!("  {}", skill.label()));
            }
            Event::Steered(input) => {
                crate::terminal_text::stderr_line(format_args!("\n> {}", input.text));
            }
            Event::Notification(Notification { text, .. }) => {
                crate::terminal_text::stderr_line(format_args!(
                    "\n{}",
                    text.lines().next().unwrap_or_default()
                ));
            }
            Event::WaitingForReset { error, delay } => {
                crate::terminal_text::stderr_line(format_args!(
                    "bart: {error}; waiting {:.0}s for reset, then continuing (Ctrl+C to cancel)",
                    delay.as_secs_f64()
                ));
            }
            Event::Retrying {
                error,
                delay,
                discarded,
            } => {
                let partial = if discarded {
                    "; its partial response is discarded"
                } else {
                    ""
                };
                crate::terminal_text::stderr_line(format_args!(
                    "bart: {error}{partial}; retrying in {:.1}s",
                    delay.as_secs_f64()
                ));
            }
            Event::RecordingStopped(reason) => {
                crate::terminal_text::stderr_line(format_args!("bart: {reason}"));
            }
            Event::Compacting => {
                crate::terminal_text::stderr_line(format_args!("[compacting context]"));
            }
            Event::Compacted { report, .. } => {
                crate::terminal_text::stderr_line(format_args!("[{report}]"));
            }
            Event::CompactionNoop => {
                crate::terminal_text::stderr_line(format_args!(
                    "[Context already has sufficient headroom.]"
                ));
            }
            Event::CompactionStage { .. }
            | Event::CompactionText(_)
            | Event::CompactionStageDone
            | Event::CompactingBytes(_)
            | Event::Delta(Delta::ToolBytes(_) | Delta::MessageBoundary)
            | Event::Accounting(_)
            | Event::ProviderObservation(_)
            | Event::Responded(None)
            | Event::Approval(_)
            | Event::Renamed(_) => {}
            Event::Responded(Some(usage)) => {
                let unknown = || "?".to_owned();
                crate::terminal_text::stderr_line(format_args!(
                    "[tokens: {} in ({} cached), {} out; cost: {}]",
                    usage.input_tokens,
                    usage
                        .cache_read_tokens
                        .map_or_else(unknown, |n| n.to_string()),
                    usage.output_tokens,
                    usage.cost.map_or_else(unknown, |cost| format!(
                        "{}${cost:.6}{}",
                        if usage.cost_is_estimate { "~" } else { "" },
                        if usage.cost_is_estimate {
                            " reported"
                        } else {
                            ""
                        }
                    ))
                ));
            }
        }
    }
}

fn summary(call: &ToolCall) -> String {
    const CHARS: usize = 160;
    let mut buffer = [0; CHARS * 4];
    let mut remaining = buffer.as_mut_slice();
    let _ = std::io::Write::write_all(&mut remaining, call.arguments.wire().as_bytes());
    let length = CHARS * 4 - remaining.len();
    let text = String::from_utf8_lossy(&buffer[..length]);
    let line: String = text.replace("\\n", " ⏎ ").chars().take(CHARS).collect();
    format!("{}({line})", call.name)
}

#[cfg(test)]
mod tests;
