use std::{
    borrow::Cow,
    fs::File,
    io::Write as _,
    os::fd::AsFd as _,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    task::JoinHandle,
    time::{Duration, timeout},
};

use super::super::{Error, IDLE_TIMEOUT, Limits, RESPONSE_BYTES, Window, prepare};
use crate::history::{Image, ToolCall, ToolResult};

const STDERR_TAIL: usize = 400;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING: usize = 4096;
const STOP: &[u8] =
    b"{\"type\":\"control_request\",\"request_id\":\"bart-stop\",\"request\":{\"subtype\":\"interrupt\"}}\n";

pub(super) struct Peer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    line: Vec<u8>,
    budget: usize,
    sequence: u64,
    pub(super) tools: Vec<Value>,
    pending: Vec<Pending>,
    cancelled: bool,
    pub(super) quota: Quota,
    stderr: Arc<Mutex<Vec<u8>>>,
    drain: Option<JoinHandle<()>>,
    stop: Arc<File>,
}

pub(super) struct Stopper(Option<Arc<File>>);

impl Stopper {
    pub(super) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Stopper {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            let _ = (&*stop).write(STOP);
        }
    }
}

#[derive(Default)]
pub(super) struct Quota {
    pub(super) rejected: bool,
    pub(super) resets_at: Option<i64>,
}

pub(super) struct Started {
    pub(super) models: Value,
    pub(super) context_window: Option<u64>,
}

pub(super) enum Frame {
    Stream(Box<RawValue>),
    Result(Value),
    Limits(Limits),
    Callback,
    Quiet,
}

struct Pending {
    call: String,
    name: String,
    arguments: Value,
    outer: String,
    inner: Value,
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    event: Option<&'a RawValue>,
}

pub(super) fn failure(message: impl Into<String>) -> Error {
    Error::Failed {
        code: "claude_cli".into(),
        message: message.into(),
    }
}

fn io_error(error: std::io::Error) -> Error {
    failure(format!("Claude Code I/O failed: {error}"))
}

fn context_changed() -> Error {
    failure("Claude Code changed context independently of Bart")
}

impl Peer {
    pub(super) fn spawn(
        args: &[String],
        cwd: Option<&Path>,
        tools: Vec<Value>,
        max_output: Option<u64>,
    ) -> Result<Self, Error> {
        let mut command = tokio::process::Command::from(crate::supervise::detached(
            &std::env::current_exe().map_err(|error| failure(error.to_string()))?,
            super::auth::executable(),
        ));
        command
            .args(args)
            .env_remove("CLAUDECODE")
            .env("DISABLE_AUTOUPDATER", "1")
            .env("DISABLE_UPDATES", "1")
            .env("DISABLE_AUTO_COMPACT", "1")
            .env("DISABLE_COMPACT", "1")
            .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
            .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
            .env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("ENABLE_TOOL_SEARCH", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        if args.iter().any(|arg| arg == "--effort") {
            command.env_remove("CLAUDE_CODE_EFFORT_LEVEL");
        }
        if let Some(limit) = max_output {
            command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", limit.to_string());
        }
        let mut child = command.spawn().map_err(|error| {
            failure(format!(
                "cannot start Claude Code: {error}; check the executable installation"
            ))
        })?;
        let input = child.stdin.take().expect("piped stdin");
        let stop = input
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| failure(format!("cannot share Claude Code input: {error}")))?;
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut pipe = child.stderr.take().expect("piped stderr");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let tail = Arc::clone(&stderr);
        let drain = tokio::spawn(async move {
            let mut chunk = [0; 1024];
            while let Ok(read @ 1..) = pipe.read(&mut chunk).await {
                let mut tail = tail.lock().expect(crate::NOT_POISONED);
                tail.extend_from_slice(&chunk[..read]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
        });
        Ok(Self {
            child,
            input,
            output,
            line: Vec::new(),
            budget: 0,
            sequence: 0,
            tools,
            pending: Vec::new(),
            cancelled: false,
            quota: Quota::default(),
            stderr,
            drain: Some(drain),
            stop: Arc::new(File::from(stop)),
        })
    }

    fn annotated(&self, message: &str) -> String {
        let tail = self.stderr.lock().expect(crate::NOT_POISONED);
        match String::from_utf8_lossy(&tail).trim() {
            "" => message.to_owned(),
            text => format!("{message}; Claude Code stderr: {text}"),
        }
    }

    async fn exited(&mut self, message: &str) -> Error {
        if let Some(drain) = self.drain.take() {
            let _ = timeout(Duration::from_secs(1), drain).await;
        }
        failure(self.annotated(message))
    }

    pub(super) fn stopper(&self) -> Stopper {
        Stopper(Some(Arc::clone(&self.stop)))
    }

    pub(super) fn reset_budget(&mut self) {
        self.budget = 0;
    }

    pub(super) async fn send_line(&mut self, line: Vec<u8>) -> Result<usize, Error> {
        match timeout(IDLE_TIMEOUT, self.input.write_all(&line)).await {
            Ok(Ok(())) => Ok(line.len()),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                Err(self.exited("Claude Code exited").await)
            }
            Ok(Err(error)) => Err(io_error(error)),
            Err(_) => Err(failure(
                self.annotated("Claude Code stopped reading its input"),
            )),
        }
    }

    async fn send(&mut self, value: &Value) -> Result<usize, Error> {
        let mut line = serde_json::to_vec(value).map_err(|error| failure(error.to_string()))?;
        line.push(b'\n');
        self.send_line(line).await
    }

    async fn read_line(&mut self) -> Result<Vec<u8>, Error> {
        let read = async {
            loop {
                let available = self.output.fill_buf().await.map_err(io_error)?;
                if available.is_empty() {
                    return Ok(false);
                }
                let end = available
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map(|index| index + 1);
                let count = end.unwrap_or(available.len());
                if self.line.len() + count > RESPONSE_BYTES {
                    return Err(Error::Limit(
                        "Claude Code JSON frame exceeds 128 MiB".into(),
                    ));
                }
                self.line.extend_from_slice(&available[..count]);
                self.output.consume(count);
                if end.is_some() {
                    return Ok(true);
                }
            }
        };
        match timeout(IDLE_TIMEOUT, read).await {
            Ok(Ok(true)) => {
                let line = std::mem::take(&mut self.line);
                self.budget = self.budget.saturating_add(line.len());
                if self.budget > RESPONSE_BYTES {
                    return Err(Error::Limit("Claude Code response exceeds 128 MiB".into()));
                }
                Ok(line)
            }
            Ok(Ok(false)) => Err(self
                .exited("Claude Code stopped before completing the response; check the installation and login with `claude`")
                .await),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Interrupted(self.annotated(&format!(
                "Claude Code sent nothing for {}s",
                IDLE_TIMEOUT.as_secs()
            )))),
        }
    }

    pub(super) async fn next(&mut self) -> Result<(Frame, usize), Error> {
        let line = self.read_line().await?;
        let invalid = |_| failure("invalid Claude Code JSON frame");
        let head: Head = serde_json::from_slice(&line).map_err(invalid)?;
        if head.kind == "stream_event" {
            let event = head
                .event
                .ok_or_else(|| failure("Claude Code stream frame without an event"))?;
            return Ok((Frame::Stream(event.to_owned()), line.len()));
        }
        let kind = head.kind.into_owned();
        let frame: Value = serde_json::from_slice(&line).map_err(invalid)?;
        let parsed = match kind.as_str() {
            "result" => Frame::Result(frame),
            "control_request" => {
                if self.incoming(&frame).await? {
                    Frame::Callback
                } else {
                    Frame::Quiet
                }
            }
            "control_cancel_request" => {
                let before = self.pending.len();
                self.pending
                    .retain(|pending| frame["request_id"] != pending.outer.as_str());
                self.cancelled |= self.pending.len() != before;
                Frame::Quiet
            }
            "rate_limit_event" => {
                let info = &frame["rate_limit_info"];
                let rejected = info["status"] == "rejected";
                self.quota = Quota {
                    rejected,
                    resets_at: info["resetsAt"].as_i64().filter(|_| rejected),
                };
                let limits = limits(info);
                if limits.short.is_some() || limits.long.is_some() {
                    Frame::Limits(limits)
                } else {
                    Frame::Quiet
                }
            }
            "system" if frame["subtype"] == "compact_boundary" => return Err(context_changed()),
            "conversation_reset" => return Err(context_changed()),
            "system" | "assistant" | "user" | "control_response" | "keep_alive"
            | "tool_progress" | "tool_use_summary" | "auth_status" | "prompt_suggestion" => {
                Frame::Quiet
            }
            _ => return Err(failure("unsupported Claude Code stdout frame")),
        };
        Ok((parsed, line.len()))
    }

    async fn controls(&mut self, requests: Vec<Value>) -> Result<Vec<Value>, Error> {
        let mut ids = Vec::with_capacity(requests.len());
        for request in &requests {
            self.sequence += 1;
            let id = format!("bart-{}", self.sequence);
            self.send(&json!({"type":"control_request","request_id":id,"request":request}))
                .await?;
            ids.push(id);
        }
        let mut answers = vec![Value::Null; ids.len()];
        let wait = async {
            let mut remaining = ids.len();
            while remaining > 0 {
                let line = self.read_line().await?;
                let frame: Value = serde_json::from_slice(&line)
                    .map_err(|_| failure("invalid Claude Code JSON frame"))?;
                match frame["type"].as_str() {
                    Some("control_response") => {
                        let response = &frame["response"];
                        let Some(index) = ids
                            .iter()
                            .position(|id| response["request_id"] == id.as_str())
                        else {
                            continue;
                        };
                        if response["subtype"] != "success" {
                            return Err(failure(format!(
                                "Claude Code rejected {}: {}",
                                requests[index]["subtype"],
                                response["error"]
                                    .as_str()
                                    .unwrap_or("unknown control error")
                            )));
                        }
                        answers[index] = response["response"].clone();
                        remaining -= 1;
                    }
                    Some("control_request") => {
                        self.incoming(&frame).await?;
                    }
                    Some("system" | "keep_alive" | "rate_limit_event") => {}
                    _ => {
                        return Err(failure(
                            "unexpected Claude Code frame while awaiting a control response",
                        ));
                    }
                }
            }
            Ok(())
        };
        match timeout(CONTROL_TIMEOUT, wait).await {
            Ok(result) => result.map(|()| answers),
            Err(_) => Err(failure(self.annotated(&format!(
                "Claude Code did not answer {} within {}s",
                requests[0]["subtype"],
                CONTROL_TIMEOUT.as_secs()
            )))),
        }
    }

    pub(super) async fn initialize(
        &mut self,
        instructions: Option<&str>,
    ) -> Result<Started, Error> {
        let mut init = json!({"subtype":"initialize","hooks":null,"skills":[]});
        if let Some(instructions) = instructions {
            init["systemPrompt"] = json!([instructions]);
        }
        let mut answers = self
            .controls(vec![
                init,
                json!({"subtype":"get_context_usage","detail":"summary"}),
            ])
            .await?;
        let context = answers.pop().unwrap_or_default();
        let mut init = answers.pop().unwrap_or_default();
        let version = init["claude_code_version"].as_str().unwrap_or("");
        let numbers: Vec<_> = version
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<_, _>>()
            .unwrap_or_default();
        if numbers.len() != 3 || numbers.as_slice() < [2, 1, 316].as_slice() {
            return Err(failure(
                "Claude Code 2.1.316 or newer is required for the stdio control bridge",
            ));
        }
        if context["isAutoCompactEnabled"] != false {
            return Err(failure(
                "Claude Code did not confirm that automatic compaction is disabled",
            ));
        }
        Ok(Started {
            models: init["models"].take(),
            context_window: context["rawMaxTokens"].as_u64().filter(|size| *size > 0),
        })
    }

    pub(super) async fn set_model(&mut self, model: &str) -> Result<(), Error> {
        self.controls(vec![json!({"subtype":"set_model","model":model})])
            .await
            .map(drop)
    }

    pub(super) async fn set_effort(&mut self, effort: Option<&str>) -> Result<(), Error> {
        self.controls(vec![
            json!({"subtype":"apply_flag_settings","settings":{"effortLevel":effort}}),
        ])
        .await
        .map(drop)
    }

    pub(super) async fn interrupt(&mut self) -> Result<(), Error> {
        self.sequence += 1;
        let id = format!("bart-{}", self.sequence);
        self.send(
            &json!({"type":"control_request","request_id":id,"request":{"subtype":"interrupt"}}),
        )
        .await?;
        let drain = async {
            loop {
                if let (Frame::Result(_), _) = self.next().await? {
                    return Ok::<_, Error>(());
                }
            }
        };
        timeout(INTERRUPT_TIMEOUT, drain)
            .await
            .map_err(|_| failure("Claude Code did not stop after an interrupt"))??;
        self.pending.clear();
        self.cancelled = false;
        Ok(())
    }

    async fn incoming(&mut self, frame: &Value) -> Result<bool, Error> {
        let request = &frame["request"];
        if request["subtype"] != "mcp_message" || request["server_name"] != "bart" {
            return Err(failure(
                "unexpected Claude Code control request; no permission was granted",
            ));
        }
        let rpc = &request["message"];
        let result = match rpc["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":rpc["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"bart","version":env!("CARGO_PKG_VERSION")}})
            }
            Some("tools/list") => json!({"tools":self.tools}),
            Some("ping" | "notifications/initialized" | "notifications/cancelled") => json!({}),
            Some("tools/call") => {
                let params = &rpc["params"];
                let call = params["_meta"]["claudecode/toolUseId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| failure("Claude Code omitted the native tool call ID"))?;
                let name = params["name"]
                    .as_str()
                    .ok_or_else(|| failure("Claude Code omitted the tool name"))?;
                if rpc["id"].is_null()
                    || self.pending.len() >= MAX_PENDING
                    || self.pending.iter().any(|pending| pending.call == call)
                    || !self.tools.iter().any(|tool| tool["name"] == name)
                {
                    return Err(failure("unexpected or duplicate Claude Code tool callback"));
                }
                self.pending.push(Pending {
                    call: call.into(),
                    name: name.into(),
                    arguments: params["arguments"].clone(),
                    outer: frame["request_id"]
                        .as_str()
                        .ok_or_else(|| failure("missing control request ID"))?
                        .into(),
                    inner: rpc["id"].clone(),
                });
                return Ok(true);
            }
            _ => return Err(failure("unsupported Claude Code MCP request")),
        };
        self.reply(
            &frame["request_id"],
            json!({"jsonrpc":"2.0","id":rpc["id"],"result":result}),
        )
        .await?;
        Ok(false)
    }

    async fn reply(&mut self, outer: &Value, response: Value) -> Result<(), Error> {
        self.send(&json!({"type":"control_response","response":{"subtype":"success","request_id":outer,"response":{"mcp_response":response}}}))
            .await
            .map(drop)
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    // Callbacks for later calls in the batch are kept until their turn.
    pub(super) async fn admit(&mut self, call: &ToolCall, batch: &[ToolCall]) -> Result<(), Error> {
        loop {
            if self.cancelled {
                return Err(failure("Claude Code cancelled a tool callback"));
            }
            if let Some(pending) = self.pending.iter().find(|pending| pending.call == call.id) {
                if pending.name != call.name || call.arguments.json() != Some(&pending.arguments) {
                    return Err(failure("Claude Code callback does not match its tool call"));
                }
                return Ok(());
            }
            if self
                .pending
                .iter()
                .any(|pending| batch.iter().all(|call| call.id != pending.call))
            {
                return Err(failure(
                    "Claude Code callback does not match an outstanding tool call",
                ));
            }
            if let (Frame::Stream(_) | Frame::Result(_), _) = self.next().await? {
                return Err(failure("Claude Code advanced before calling its tool"));
            }
        }
    }

    pub(super) async fn answer(
        &mut self,
        result: &ToolResult,
        images: Vec<Image>,
        notes: Vec<String>,
    ) -> Result<(), Error> {
        let index = self
            .pending
            .iter()
            .position(|pending| pending.call == result.call_id)
            .filter(|_| !self.cancelled)
            .ok_or_else(|| failure("Claude Code cancelled a tool callback"))?;
        let pending = self.pending.remove(index);
        let output = result.output.clone();
        let is_error = result.is_error;
        let slot = prepare::slot().await;
        let line = slot
            .run(move || {
                let mut content = vec![json!({"type":"text","text":output})];
                for image in images {
                    if !image.label.is_empty() {
                        content.push(json!({"type":"text","text":image.label}));
                    }
                    content.push(json!({"type":"image","data":image.base64().map_err(io_error)?,"mimeType":image.media_type}));
                }
                content.extend(notes.into_iter().map(|text| json!({"type":"text","text":text})));
                let mut line = serde_json::to_vec(&json!({"type":"control_response","response":{"subtype":"success","request_id":pending.outer,"response":{"mcp_response":{"jsonrpc":"2.0","id":pending.inner,"result":{"content":content,"isError":is_error}}}}}))
                    .map_err(|error| failure(error.to_string()))?;
                line.push(b'\n');
                Ok(line)
            })
            .await?;
        self.send_line(line).await.map(drop)
    }

    pub(super) async fn stop(mut self) {
        kill_group(&self.child);
        let _ = self.child.wait().await;
    }
}

pub(super) fn kill_group(child: &Child) {
    if let Some(pid) = child
        .id()
        .and_then(|pid| rustix::process::Pid::from_raw(pid.cast_signed()))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        kill_group(&self.child);
    }
}

fn limits(info: &Value) -> Limits {
    let window = |name: &str, minutes: u64| {
        let utilization = info["unifiedWindows"][name]["utilization"]
            .as_f64()
            .or_else(|| {
                (info["rateLimitType"] == name)
                    .then(|| info["utilization"].as_f64())
                    .flatten()
            })?;
        Some(Window {
            used_percent: utilization * 100.0,
            minutes: Some(minutes),
        })
    };
    Limits {
        short: window("five_hour", 5 * 60),
        long: window("seven_day", 7 * 24 * 60),
    }
}
