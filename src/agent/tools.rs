use std::time::Instant;

use serde_json::Value;

use super::{Error, Event, MAX_RESPONSE_IMAGES, Reporter, ReviewReporter, Session};
use crate::{
    approval::Mode,
    history::{Message, ToolCall, ToolResult},
    tool::{self, Output},
};

impl Session {
    /// Returns whether a review stopped the turn. A provider failure leaves the
    /// remaining calls recorded as not run.
    pub(super) async fn run_calls(
        &mut self,
        calls: &[ToolCall],
        on_event: &mut impl Reporter,
    ) -> Result<bool, Error> {
        let mut stopped = false;
        let sees_images = self.routing.sees_images();
        let mut images = Vec::new();
        let mut failed = None;
        for (index, call) in calls.iter().enumerate() {
            if let Err(error) = self.routing.transport.admit(call).await {
                self.not_run(&calls[index..], &error, on_event).await;
                failed = Some(error);
                break;
            }
            let pending: usize = images.iter().map(crate::history::Image::history_len).sum();
            let full = Self::check_history_size(self.journal.bytes().saturating_add(pending)).err();
            let code = call.arguments.get("code").and_then(Value::as_str);
            let mut live = self
                .context
                .live
                .start(&call.name, code.unwrap_or_default(), None);
            on_event.event(Event::ToolStarted {
                call,
                live: live.id(),
            });
            on_event.flush().await;
            let started = Instant::now();
            let mut output = if let Some(error) = full {
                Output::error(format!("[not run: {error}]"))
            } else if stopped {
                Output::error("[not run: parent denied: earlier review stopped this turn]".into())
            } else if self
                .routing
                .preferences()
                .effective
                .permissions
                .approval
                .mode()
                != Mode::None
                || self.context.confinement().jailed()
            {
                let (output, stop) = self.reviewed_call(call, &live, on_event).await;
                stopped = stop;
                output
            } else {
                tool::run(call, &self.context, &live, None).await
            };
            let room = if sees_images {
                MAX_RESPONSE_IMAGES.saturating_sub(images.len())
            } else {
                0
            };
            images.extend(tool::deliver(&mut output, &self.context, room, sees_images).await);
            live.result(&output.text);
            live.finish(output.status);
            output.activities = live.activities();
            on_event.event(Event::ToolFinished {
                call,
                output: &output,
                elapsed: started.elapsed(),
            });
            let result = ToolResult {
                diffs: std::mem::take(&mut output.diffs),
                activities: std::mem::take(&mut output.activities),
                call_id: call.id.clone(),
                is_error: output.is_error(),
                output: output.text.into(),
                jobs: output.jobs,
                saved: output.saved,
                cell: output.cell,
                notes: output.notes,
            };
            self.push(Message::ToolResult(result.clone()), on_event)
                .await;
            on_event.flush().await;
            if let Err(error) = self.routing.transport.answer(&result).await {
                self.not_run(&calls[index + 1..], &error, on_event).await;
                failed = Some(error);
                break;
            }
        }
        for image in images {
            self.push(Message::Image(image), on_event).await;
        }
        self.report_jobs(on_event).await;
        failed.map_or(Ok(stopped), |error| Err(Error::Model(error)))
    }

    async fn not_run(
        &mut self,
        calls: &[ToolCall],
        error: &crate::provider::Error,
        on_event: &mut impl Reporter,
    ) {
        for call in calls {
            let result = ToolResult {
                diffs: Vec::new(),
                activities: crate::tool::Activities::default(),
                call_id: call.id.clone(),
                is_error: true,
                output: format!("[not run: {error}]").into(),
                jobs: Vec::new(),
                saved: None,
                cell: None,
                notes: String::new(),
            };
            self.push(Message::ToolResult(result), on_event).await;
        }
    }

    async fn reviewed_call(
        &mut self,
        call: &ToolCall,
        live: &tool::Run,
        on_event: &mut impl Reporter,
    ) -> (Output, bool) {
        let mut report = ReviewReporter { reporter: on_event };
        let mut gate = crate::approval::Gate {
            client: &self.routing.client,
            store: &self.routing.store,
            state: &mut self.reviewer,
            journal: &mut self.journal,
            interactive: self.interactive,
            report: &mut report,
            stopped: false,
            approval: &self.routing.preferences().effective.permissions.approval,
        };
        let output = Box::pin(tool::run(call, &self.context, live, Some(&mut gate))).await;
        (output, gate.stopped)
    }
}
