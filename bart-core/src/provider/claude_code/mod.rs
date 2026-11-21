use std::path::PathBuf;

use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{
    Error, Model, Observation, Observer, Protocol, Request, StatusCode, Tools, Trace, Visibility,
    deliver, observe, prepare, quota_retry, unix_now, validate,
};
use crate::history::{Assistant, Content, Message, Part, ToolCall, ToolResult};
use wire::{Frame, Peer, failure};

mod auth;
mod output;
mod wire;

const TOOL_PREFIX: &str = "mcp__bart__";

pub async fn available() -> bool {
    auth::authenticated().await.is_ok()
}

#[derive(Default)]
pub(super) struct Route {
    cwd: Option<PathBuf>,
    connection: Mutex<Option<Connection>>,
}

struct Connection {
    peer: Peer,
    identity: Value,
    model: String,
    effort: Option<String>,
    history: Vec<Message>,
    active: Option<ToolCall>,
    output: output::Output,
    reported_cost: f64,
    state: State,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Sending,
    Streaming,
}

impl Route {
    pub(super) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd: Some(cwd),
            ..Self::default()
        }
    }

    pub(super) fn isolated(&self) -> Self {
        Self {
            cwd: self.cwd.clone(),
            ..Self::default()
        }
    }

    pub(super) async fn close(&self) {
        if let Some(connection) = self.connection.lock().await.take() {
            connection.peer.stop().await;
        }
    }

    async fn ready<'a>(
        &self,
        slot: &'a mut Option<Connection>,
        request: &Request<'_>,
    ) -> Result<&'a mut Connection, Error> {
        let (tools, instructions) = prepare(request)?;
        let identity = identity(request, &tools, &instructions);
        if let Some(connection) = slot.as_mut() {
            let reusable = connection.identity == identity
                && connection.settle().await
                && connection.matches(request.history);
            if !reusable && let Some(old) = slot.take() {
                old.peer.stop().await;
            }
        }
        if slot.is_none() {
            let mut argv = args(Some(request.model), &tools);
            argv.extend([
                "--thinking-display".into(),
                if request.summaries {
                    "summarized"
                } else {
                    "omitted"
                }
                .into(),
            ]);
            if let Some(effort) = request.reasoning_effort {
                argv.extend(["--effort".into(), effort.into()]);
            }
            let mut peer =
                Peer::spawn(&argv, self.cwd.as_deref(), tools, request.max_output_tokens)?;
            peer.initialize(Some(&instructions)).await?;
            *slot = Some(Connection {
                peer,
                identity,
                model: request.model.into(),
                effort: request.reasoning_effort.map(str::to_owned),
                history: Vec::new(),
                active: None,
                output: output::Output::default(),
                reported_cost: 0.0,
                state: State::Idle,
            });
        }
        Ok(slot.as_mut().expect("the connection was just ensured"))
    }

    pub(super) async fn admit(&self, call: &ToolCall) -> Result<Option<CancellationToken>, Error> {
        let mut slot = self.connection.lock().await;
        let Some(connection) = slot.as_mut() else {
            return Ok(None);
        };
        let admitted = connection.admit(call).map(Some);
        if admitted.is_err()
            && let Some(connection) = slot.take()
        {
            connection.peer.stop().await;
        }
        admitted
    }

    pub(super) async fn answer(&self, result: &ToolResult) -> Result<(), Error> {
        let mut slot = self.connection.lock().await;
        let Some(connection) = slot.as_mut() else {
            return Ok(());
        };
        let answered = connection.check_result(result);
        if answered.is_err()
            && let Some(connection) = slot.take()
        {
            connection.peer.stop().await;
        }
        answered
    }
}

fn prepare(request: &Request<'_>) -> Result<(Vec<Value>, String), Error> {
    let definitions = match request.tools {
        Tools::Functions(tools) | Tools::Disabled(tools) => tools,
        Tools::WebSearch => {
            return Err(failure(
                "Claude Code native web search is not available through Bart",
            ));
        }
    };
    let mut instructions = format!(
        "{}\nBart owns execution. Its Python tool is named mcp__bart__python in Claude Code. Use that exact tool name; other MCP services are reached through Python, not as native Claude Code tools.",
        request.instructions
    );
    let mut tools = Vec::with_capacity(definitions.len());
    for tool in definitions {
        instructions.push_str(&format!(
            "\n\nTool reference for {TOOL_PREFIX}{}:\n{}",
            tool.name, tool.description
        ));
        tools.push(json!({"name":tool.name,"description":format!("Run Bart's {} tool. Its complete contract is in the system instructions.", tool.name),"inputSchema":tool.parameters}));
    }
    if matches!(request.tools, Tools::Disabled(_)) {
        instructions.push_str("\nTool definitions are reference only for this request. Do not call tools; respond with text only.");
        tools.clear();
    }
    Ok((tools, instructions))
}

fn args(model: Option<&str>, tools: &[Value]) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources=user",
        "--settings",
        "{\"disableAllHooks\":true}",
        "--disable-slash-commands",
        "--mcp-config",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(
        if tools.is_empty() {
            json!({"mcpServers":{}})
        } else {
            json!({"mcpServers":{"bart":{"type":"sdk","name":"bart"}}})
        }
        .to_string(),
    );
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if !tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .map(|name| format!("{TOOL_PREFIX}{name}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    args
}

fn identity(request: &Request<'_>, tools: &[Value], instructions: &str) -> Value {
    json!([
        instructions,
        matches!(request.tools, Tools::Disabled(_)),
        tools,
        request.summaries,
        request.max_output_tokens,
        request.cache.map(|cache| (cache.key, cache.window))
    ])
}

impl Route {
    pub(super) async fn send(
        &self,
        request: &Request<'_>,
        observer: &mut impl Observer,
        trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error> {
        let mut slot = self.connection.lock().await;
        let result = async {
            self.ready(&mut slot, request)
                .await?
                .exchange(request, observer, trace)
                .await
        }
        .await;
        if result.is_err()
            && let Some(connection) = slot.take()
        {
            connection.peer.stop().await;
        }
        result
    }

    pub(super) async fn prewarm(&self, request: &Request<'_>) -> Result<(), Error> {
        let mut slot = self.connection.lock().await;
        let ready = self.ready(&mut slot, request).await.map(drop);
        if ready.is_err() {
            *slot = None;
        }
        ready
    }
}

impl Connection {
    async fn settle(&mut self) -> bool {
        match self.state {
            State::Idle => true,
            State::Sending => false,
            State::Streaming => {
                let stopped = self.peer.interrupt().await.is_ok();
                self.active = None;
                self.output = output::Output::default();
                self.state = State::Idle;
                stopped
            }
        }
    }

    fn matches(&self, history: &[Message]) -> bool {
        if !history.starts_with(&self.history) {
            return false;
        }
        let suffix = &history[self.history.len()..];
        if suffix
            .iter()
            .any(|message| matches!(message, Message::Assistant(_)))
        {
            return false;
        }
        let Some(call) = &self.active else {
            return true;
        };
        suffix.iter().all(result_attachment)
            && suffix.iter().filter(|message| matches!(message, Message::ToolResult(_))).count() == 1
            && suffix.iter().any(|message| matches!(message, Message::ToolResult(result) if result.call_id == call.id))
    }

    fn admit(&self, call: &ToolCall) -> Result<CancellationToken, Error> {
        if self.state != State::Idle || self.active.as_ref() != Some(call) {
            return Err(failure(
                "Bart's tool call is not Claude Code's outstanding request",
            ));
        }
        self.peer.admit(call)
    }

    fn check_result(&self, result: &ToolResult) -> Result<(), Error> {
        if self.state != State::Idle
            || self
                .active
                .as_ref()
                .is_none_or(|call| call.id != result.call_id)
        {
            return Err(failure(
                "Bart's result is not for Claude Code's outstanding request",
            ));
        }
        Ok(())
    }

    async fn exchange(
        &mut self,
        request: &Request<'_>,
        observer: &mut impl Observer,
        mut trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error> {
        if self.model != request.model {
            self.peer.set_model(request.model).await?;
            request.model.clone_into(&mut self.model);
        }
        if self.effort.as_deref() != request.reasoning_effort {
            self.peer.set_effort(request.reasoning_effort).await?;
            self.effort = request.reasoning_effort.map(str::to_owned);
        }
        self.peer.reset_turn();
        self.state = State::Sending;
        let suffix = &request.history[self.history.len()..];
        let parked = self.active.is_some();
        let fresh = |message: &&Message| !parked || !result_attachment(message);
        if suffix.iter().any(|message| fresh(&message)) {
            let slot = prepare::slot().await;
            let input: Vec<_> = suffix.iter().filter(fresh).cloned().collect();
            let frame = slot.run(move || project(&input)).await?;
            let sent = self.peer.send_line(frame).await?;
            if let Some(trace) = &mut trace {
                trace.body(sent);
            }
        } else if !parked {
            return Err(failure("Claude Code has no new input"));
        }
        if parked {
            tokio::time::timeout(super::IDLE_TIMEOUT, self.release(suffix))
                .await
                .map_err(|_| {
                    failure(format!(
                        "Claude Code did not accept the tool results within {} seconds",
                        super::IDLE_TIMEOUT.as_secs()
                    ))
                })??;
        }
        self.history = request.history.to_vec();
        self.state = State::Streaming;
        let stopper = self.peer.stopper();
        let response = self.respond(request, observer, trace).await;
        stopper.disarm();
        let assistant = response?;
        self.history.push(Message::Assistant(assistant.clone()));
        self.state = State::Idle;
        Ok(assistant)
    }

    async fn release(&mut self, suffix: &[Message]) -> Result<(), Error> {
        let images = suffix
            .iter()
            .filter_map(|message| match message {
                Message::Image(image) => Some(image.clone()),
                _ => None,
            })
            .collect();
        let notes = suffix
            .iter()
            .filter_map(|message| match message {
                Message::Notification(note) => Some(note.text.to_string()),
                _ => None,
            })
            .collect();
        let call = self
            .active
            .as_ref()
            .ok_or_else(|| failure("no outstanding Claude Code request"))?;
        let result = suffix
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.call_id == call.id => Some(result),
                _ => None,
            })
            .ok_or_else(|| failure("missing Bart tool result"))?;
        self.peer.answer(result, images, notes).await?;
        self.active = None;
        Ok(())
    }

    async fn respond(
        &mut self,
        request: &Request<'_>,
        observer: &mut impl Observer,
        mut trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error> {
        loop {
            let (frame, size) = observe(self.peer.next(), trace.as_deref_mut()).await?;
            if let Some(trace) = &mut trace {
                trace.chunk(size);
            }
            match frame {
                Frame::Quiet | Frame::Control(_) => {}
                Frame::Limits(limits) => {
                    observer.observation(Observation::Limits(limits));
                    observer.flush().await;
                }
                Frame::Stream(event) => {
                    self.output
                        .event(&event, request, observer, trace.as_deref_mut())?;
                    observer.flush().await;
                }
                Frame::Assistant(frame) => self.output.assistant(&frame)?,
                Frame::Call(call) => {
                    if !self.peer.is_pending(&call.id) {
                        continue;
                    }
                    let mut assistant = self.output.take(request, false);
                    assistant.parts.push(Part {
                        content: Content::ToolCall(call.clone()),
                        replay: None,
                    });
                    validate(request, &assistant)?;
                    self.active = Some(call);
                    return Ok(assistant);
                }
                Frame::Result(frame) => {
                    if frame["is_error"] == true || frame["subtype"] != "success" {
                        return Err(self.failed(&frame, trace));
                    }
                    self.peer.finish_turn()?;
                    let mut assistant = self.output.take(request, true);
                    if let Some(total) = frame["total_cost_usd"]
                        .as_f64()
                        .filter(|total| total.is_finite() && *total >= self.reported_cost)
                    {
                        if let Some(usage) = &mut assistant.usage {
                            usage.cost = Some(total - self.reported_cost);
                            usage.cost_is_estimate = true;
                        }
                        self.reported_cost = total;
                    }
                    return Ok(assistant);
                }
            }
        }
    }

    fn failed(&self, frame: &Value, trace: Option<&mut Trace>) -> Error {
        let message = frame["result"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map_or_else(|| frame["errors"].to_string(), str::to_owned);
        if message.contains("prompt is too long") || message.contains("context_length_exceeded") {
            return Error::ContextOverflow(message);
        }
        let quota = &self.peer.quota;
        let status = frame["api_error_status"]
            .as_u64()
            .and_then(|status| u16::try_from(status).ok())
            .and_then(|status| StatusCode::from_u16(status).ok())
            .or(quota.rejected.then_some(StatusCode::TOO_MANY_REQUESTS))
            .filter(|status| !status.is_success());
        let Some(status) = status else {
            return failure(format!(
                "Claude Code returned {}: {message}",
                frame["subtype"]
            ));
        };
        let usage_limit = quota.rejected && status == StatusCode::TOO_MANY_REQUESTS;
        let resets_at = quota.resets_at.filter(|_| usage_limit);
        let retry = quota_retry(status, None, resets_at, usage_limit, unix_now());
        if let Some(trace) = trace {
            trace.provider_error(
                false,
                Some(status.as_u16()),
                usage_limit,
                resets_at.is_some(),
                false,
                super::retry_kind(retry),
            );
        }
        Error::Http {
            status,
            message,
            retry,
        }
    }
}

fn result_attachment(message: &Message) -> bool {
    matches!(
        message,
        Message::ToolResult(_) | Message::Image(_) | Message::Notification(_)
    )
}

fn project(history: &[Message]) -> Result<Vec<u8>, Error> {
    let mut blocks = Vec::new();
    for message in history {
        let text = match message {
            Message::User(_)
            | Message::UserSource(_)
            | Message::Instruction(_)
            | Message::Skill(_)
            | Message::Notification(_) => message.model_text().into_owned(),
            Message::Assistant(assistant) => {
                let parts: Vec<_> = assistant
                    .parts
                    .iter()
                    .filter_map(|part| match &part.content {
                        Content::Text(_) | Content::ToolCall(_) => Some(&part.content),
                        Content::Reasoning(_) | Content::Other => None,
                    })
                    .collect();
                format!(
                    "{}\n{}",
                    bart!(History, "assistant message from {}", assistant.model),
                    json!(parts)
                )
            }
            Message::ToolResult(result) => format!(
                "{}\n{}",
                bart!(
                    History,
                    "result of call {}{}",
                    result.call_id,
                    if result.is_error { "; failed" } else { "" }
                ),
                result.output
            ),
            Message::Image(image) => {
                if !image.label.is_empty() {
                    blocks.push(json!({"type":"text","text":image.label}));
                }
                blocks.push(json!({"type":"image","source":{"type":"base64","media_type":image.media_type,"data":image.base64().map_err(|error| failure(error.to_string()))?}}));
                continue;
            }
        };
        blocks.push(json!({"type":"text","text":text}));
    }
    let mut frame = json!({"type":"user","client_composed":true,"message":{"role":"user"},"parent_tool_use_id":null});
    frame["message"]["content"] = Value::Array(blocks);
    let mut line = serde_json::to_vec(&frame).map_err(|error| failure(error.to_string()))?;
    line.push(b'\n');
    Ok(line)
}

fn listing(started: wire::Started, selected: Option<&str>) -> Result<Vec<Model>, Error> {
    let available = started
        .models
        .as_array()
        .ok_or_else(|| failure("Claude Code did not report its models"))?;
    let mut models: Vec<Model> = Vec::new();
    for entry in available {
        let alias = entry["value"]
            .as_str()
            .ok_or_else(|| failure("Claude Code returned an unnamed model"))?;
        let id = entry["resolvedModel"].as_str().unwrap_or(alias);
        let mut model = if let Some(model) = models.iter().find(|model| model.id == id) {
            model.clone()
        } else {
            let efforts = entry["supportedEffortLevels"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let model = Model {
                id: id.into(),
                visibility: Visibility::Listed,
                name: entry["description"].as_str().map(str::to_owned),
                efforts,
                default_effort: None,
                summaries: Some(true),
                verbosity: None,
                images: None,
                context_window: None,
                service_tiers: None,
                max_context_window: None,
                max_output_tokens: None,
            };
            models.push(model.clone());
            model
        };
        if alias != id {
            model.id = alias.into();
            model.visibility = Visibility::Unlisted;
            models.push(model);
        }
    }
    if let (Some(selected), Some(window)) = (selected, started.context_window) {
        for model in models.iter_mut().filter(|model| model.id == selected) {
            model.context_window = Some(window);
            model.max_context_window = Some(window);
        }
    }
    Ok(models)
}

pub(super) async fn models(selected: Option<&str>) -> Result<Vec<Model>, Error> {
    let mut peer = Peer::spawn(&args(selected, &[]), None, Vec::new(), None)?;
    let started = peer.initialize(None).await?;
    peer.stop().await;
    listing(started, selected)
}

pub(super) async fn context_window(model: &str) -> Result<u64, Error> {
    let mut peer = Peer::spawn(&args(Some(model), &[]), None, Vec::new(), None)?;
    let started = peer.initialize(None).await?;
    peer.stop().await;
    started
        .context_window
        .ok_or_else(|| failure("Claude Code did not report the model's context window"))
}
