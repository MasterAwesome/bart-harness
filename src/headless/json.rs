use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Output, stdin_lines};
use crate::{
    agent::{self, Event, Session},
    history::NotificationKind,
    provider::Delta,
    signals::Terminations,
    tool::Status,
};

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    User { text: String },
    Interrupt,
    Compact,
}

type Lines = Option<mpsc::Receiver<Result<String, String>>>;

struct Writer {
    output: Output,
    message: String,
    response_start: usize,
}

impl Output {
    fn emit(&self, mut event: Value, kind: &str) {
        event["type"] = kind.into();
        self.write(&format!("{event}\n"));
    }
}

impl Writer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Delta(Delta::Text(text)) => {
                self.message.push_str(text);
                self.output.emit(json!({"delta": text}), "text");
            }
            Event::Delta(Delta::Reasoning(text)) => {
                self.output.emit(json!({"delta": text}), "reasoning")
            }
            Event::ToolStarted { call, .. } => {
                self.message.clear();
                self.response_start = 0;
                let code = serde_json::from_str::<Value>(call.arguments.wire())
                    .ok()
                    .and_then(|arguments| arguments.get("code").cloned());
                self.output
                    .emit(json!({"id": call.id, "code": code}), "tool_started");
            }
            Event::ToolFinished {
                call,
                output,
                elapsed,
            } => self.output.emit(
                json!({
                    "id": call.id,
                    "cell": output.cell,
                    "ok": output.status == Status::Succeeded,
                    "output": output.text,
                    "jobs_started": output.jobs,
                    "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                }),
                "tool_finished",
            ),
            Event::Notification(notification) => {
                let mut event = serde_json::to_value(&notification.kind).unwrap_or_default();
                let kind = match notification.kind {
                    NotificationKind::Recovery => "recovery",
                    NotificationKind::JobLost { .. } => "job_lost",
                    NotificationKind::JobDetached { .. } => "job_detached",
                    NotificationKind::JobFinished { .. } => "job_finished",
                    NotificationKind::JobMatched { .. } => "job_matched",
                    NotificationKind::JobCommandExited { .. } => "job_command_exited",
                };
                if let Some(fields) = event.as_object_mut() {
                    fields.remove("kind");
                    fields.remove("capture");
                }
                event["text"] = (&*notification.text).into();
                self.output.emit(event, kind);
            }
            Event::Responded(usage) => {
                self.response_start = self.message.len();
                if let Some(usage) = usage {
                    self.output
                        .emit(serde_json::to_value(usage).unwrap_or_default(), "usage");
                }
            }
            Event::Steered(input) => self.output.emit(json!({"text": input.text}), "steered"),
            Event::Image(image) => self.output.emit(json!({"path": image.path}), "image"),
            Event::Compacted { report, .. } => {
                self.output.emit(json!({"report": report}), "compacted")
            }
            Event::Retrying {
                error,
                delay,
                discarded,
            } => self.retry(error, delay, discarded),
            Event::WaitingForReset { error, delay } => self.retry(error, delay, true),
            Event::RecordingStopped(reason) => self.output.emit(json!({"text": reason}), "warning"),
            _ => {}
        }
    }

    fn retry(
        &mut self,
        error: &crate::provider::Error,
        delay: std::time::Duration,
        discarded: bool,
    ) {
        if discarded {
            self.message.truncate(self.response_start);
        }
        self.output.emit(
            json!({"error": error.to_string(), "delay_ms": u64::try_from(delay.as_millis()).unwrap_or(u64::MAX), "discarded": discarded, "message": self.message}),
            "retrying",
        );
    }
}

enum Work {
    Turn(String),
    Wake,
    Compact,
}

enum Ended {
    Done(Result<(), agent::Error>),
    Interrupted,
    Stop,
}

pub(super) async fn session(
    session: &mut Session,
    id: &str,
    terminations: &mut Terminations,
) -> Result<(), String> {
    session.prewarm().await;
    let output = Output::default();
    let mut writer = Writer {
        output: output.clone(),
        message: String::new(),
        response_start: 0,
    };
    let settings = session.settings();
    output.emit(
        json!({
            "id": id,
            "provider": settings.provider.name(),
            "model": settings.model,
            "effort": settings.reasoning_effort,
            "cwd": session.tool_context().cwd,
        }),
        "session",
    );
    let mut lines: Lines = Some(stdin_lines()?);
    while lines.is_some() {
        let work = tokio::select! {
            line = next(&mut lines) => match line? {
                Some(Request::User { text }) => Work::Turn(text),
                Some(Request::Compact) => Work::Compact,
                Some(Request::Interrupt) => continue,
                None => continue,
            },
            () = session.jobs_ready(), if session.has_jobs() => Work::Wake,
            _ = terminations.requested() => return Ok(()),
            error = output.failed() => return Err(error),
        };
        let cause = match work {
            Work::Turn(_) => "user",
            Work::Wake => "job",
            Work::Compact => "compact",
        };
        output.emit(json!({"cause": cause}), "turn_started");
        writer.message.clear();
        writer.response_start = 0;
        let inbox = session.inbox();
        let ended = {
            let report = |event: Event<'_>| writer.event(event);
            let turn = async {
                match work {
                    Work::Turn(text) => session.turn(text.into(), report).await,
                    Work::Wake => session.wake(report).await,
                    Work::Compact => session.compact(report).await,
                }
            };
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    result = &mut turn => break Ended::Done(result),
                    line = next(&mut lines), if lines.is_some() => match line {
                        Ok(Some(Request::User { text })) => {
                            if inbox.push(text.into()).is_err() {
                                output.emit(json!({"request": "user", "error": agent::INPUT_FULL}), "request_rejected");
                            }
                        }
                        Ok(Some(Request::Interrupt)) => break Ended::Interrupted,
                        Ok(Some(Request::Compact)) => output.emit(
                            json!({"request": "compact", "error": "a turn is running; retry after turn_done"}),
                            "request_rejected",
                        ),
                        Ok(None) => break Ended::Stop,
                        Err(error) => return Err(error),
                    },
                    _ = terminations.requested() => break Ended::Stop,
                    error = output.failed() => return Err(error),
                }
            }
        };
        let (status, error) = match &ended {
            Ended::Done(Ok(())) => ("completed", None),
            Ended::Done(Err(error)) => ("failed", Some(error.to_string())),
            Ended::Interrupted | Ended::Stop => ("interrupted", None),
        };
        output.emit(
            json!({
                "status": status,
                "error": error,
                "message": writer.message,
                "jobs_running": session.jobs().running_ids(),
            }),
            "turn_done",
        );
        if let Some(error) = output.failure() {
            return Err(error);
        }
        if matches!(ended, Ended::Stop) {
            return Ok(());
        }
    }
    output.failure().map_or(Ok(()), Err)
}

async fn next(lines: &mut Lines) -> Result<Option<Request>, String> {
    let Some(receiver) = lines else {
        return std::future::pending().await;
    };
    loop {
        let Some(line) = receiver.recv().await.transpose()? else {
            *lines = None;
            return Ok(None);
        };
        if line.trim().is_empty() {
            continue;
        }
        return serde_json::from_str(&line)
            .map(Some)
            .map_err(|error| format!("invalid input line: {error}"));
    }
}
