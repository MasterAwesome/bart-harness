use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::Write as _,
    os::fd::AsFd as _,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use rmcp::model::{
    CallToolRequestParams, InitializeRequestParams, JsonRpcMessage, PaginatedRequestParams,
    ProtocolVersion,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc,
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
    output: mpsc::UnboundedReceiver<Result<(Value, usize), Error>>,
    buffered: Arc<AtomicUsize>,
    reader: JoinHandle<()>,
    queued: VecDeque<(Frame, usize)>,
    budget: usize,
    sequence: u64,
    pub(super) tools: Vec<Value>,
    pending: Vec<Pending>,
    cancelled: Arc<Mutex<Cancellations>>,
    issued: HashSet<String>,
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
    Stream(Value),
    Assistant(Value),
    Control(Value),
    Result(Value),
    Limits(Limits),
    Call(ToolCall),
    Quiet,
}

struct Pending {
    call: ToolCall,
    outer: String,
    inner: Value,
    cancellation: CancellationToken,
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

#[derive(Default)]
struct Cancellations {
    control: HashSet<String>,
    rpc: HashSet<String>,
    controls: HashMap<String, CancellationToken>,
    requests: HashMap<String, CancellationToken>,
}

impl Cancellations {
    fn contains(&self, outer: &str, inner: &Value) -> bool {
        self.control.contains(outer) || self.rpc.contains(&inner.to_string())
    }
}

fn tool_error(message: &str) -> Value {
    json!({"content":[{"type":"text","text":message}],"isError":true})
}

#[derive(Deserialize)]
struct Rpc {
    method: String,
    params: Option<Map<String, Value>>,
}

fn params<T: DeserializeOwned>(rpc: &Rpc) -> Result<T, Value> {
    serde_json::from_value(rpc.params.clone().map(Value::Object).unwrap_or_default()).map_err(
        |error| json!({"code":-32602,"message":format!("Invalid request parameters: {error}")}),
    )
}

async fn read_frames(
    mut output: BufReader<ChildStdout>,
    sender: mpsc::UnboundedSender<Result<(Value, usize), Error>>,
    buffered: Arc<AtomicUsize>,
    cancelled: Arc<Mutex<Cancellations>>,
) {
    loop {
        let frame = async {
            let mut line = Vec::new();
            loop {
                let bytes = output.fill_buf().await.map_err(io_error)?;
                if bytes.is_empty() {
                    return Ok(None);
                }
                let end = bytes
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map(|index| index + 1);
                let count = end.unwrap_or(bytes.len());
                if line.len() + count > RESPONSE_BYTES {
                    return Err(Error::Limit(
                        "Claude Code JSON frame exceeds 128 MiB".into(),
                    ));
                }
                line.extend_from_slice(&bytes[..count]);
                output.consume(count);
                if end.is_some() {
                    break;
                }
            }
            let frame: Value = serde_json::from_slice(&line)
                .map_err(|_| failure("invalid Claude Code JSON frame"))?;
            if frame["type"] == "control_cancel_request"
                && let Some(id) = frame["request_id"].as_str()
            {
                let mut cancelled = cancelled.lock().expect(crate::NOT_POISONED);
                if cancelled.control.len() >= MAX_PENDING {
                    return Err(failure("too many Claude Code cancellations"));
                }
                cancelled.control.insert(id.to_owned());
                if let Some(token) = cancelled.controls.get(id) {
                    token.cancel();
                }
            }
            if frame["type"] == "control_request"
                && frame["request"]["subtype"] == "mcp_message"
                && frame["request"]["server_name"] == "bart"
                && frame["request"]["message"]["method"] == "notifications/cancelled"
            {
                let id = &frame["request"]["message"]["params"]["requestId"];
                if !id.is_null() {
                    let mut cancelled = cancelled.lock().expect(crate::NOT_POISONED);
                    if cancelled.rpc.len() >= MAX_PENDING {
                        return Err(failure("too many Claude Code MCP cancellations"));
                    }
                    cancelled.rpc.insert(id.to_string());
                    if let Some(token) = cancelled.requests.get(&id.to_string()) {
                        token.cancel();
                    }
                }
            }
            Ok(Some((frame, line.len())))
        }
        .await;
        let error = match frame {
            Ok(Some((frame, size))) => {
                let previous = buffered.fetch_add(size, Ordering::Relaxed);
                if previous.saturating_add(size) > RESPONSE_BYTES {
                    Some(Error::Limit(
                        "Claude Code unread output exceeds 128 MiB".into(),
                    ))
                } else if sender.send(Ok((frame, size))).is_err() {
                    return;
                } else {
                    continue;
                }
            }
            Ok(None) => None,
            Err(error) => Some(error),
        };
        for token in cancelled
            .lock()
            .expect(crate::NOT_POISONED)
            .controls
            .values()
        {
            token.cancel();
        }
        if let Some(error) = error {
            let _ = sender.send(Err(error));
        }
        return;
    }
}

impl Peer {
    pub(super) fn spawn(
        args: &[String],
        cwd: Option<&Path>,
        tools: Vec<Value>,
        max_output: Option<u64>,
    ) -> Result<Self, Error> {
        let mut command = tokio::process::Command::from(
            crate::supervise::detached(super::auth::executable())
                .map_err(|error| failure(error.to_string()))?,
        );
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
                "cannot start Claude Code process wrapper: {error}; check Bart’s launcher and the session working directory"
            ))
        })?;
        let input = child.stdin.take().expect("piped stdin");
        let stop = input
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| failure(format!("cannot share Claude Code input: {error}")))?;
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let (sender, output) = mpsc::unbounded_channel();
        let buffered = Arc::new(AtomicUsize::new(0));
        let buffer_size = Arc::clone(&buffered);
        let cancelled = Arc::new(Mutex::new(Cancellations::default()));
        let cancellations = Arc::clone(&cancelled);
        let reader = tokio::spawn(async move {
            read_frames(stdout, sender, buffer_size, cancellations).await;
        });
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
            buffered,
            reader,
            queued: VecDeque::new(),
            budget: 0,
            sequence: 0,
            tools,
            pending: Vec::new(),
            cancelled,
            issued: HashSet::new(),
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

    pub(super) fn reset_turn(&mut self) {
        self.budget = 0;
        self.quota = Quota::default();
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

    async fn read_frame(&mut self) -> Result<(Value, usize), Error> {
        let line = timeout(IDLE_TIMEOUT, self.output.recv())
            .await
            .map_err(|_| {
                Error::Interrupted(self.annotated(&format!(
                    "Claude Code sent nothing for {}s",
                    IDLE_TIMEOUT.as_secs()
                )))
            })?;
        let Some(line) = line else {
            return Err(self.exited("Claude Code stopped before completing the response; check the installation and login with `claude`").await);
        };
        let (frame, size) = line?;
        self.buffered.fetch_sub(size, Ordering::Relaxed);
        self.budget = self.budget.saturating_add(size);
        if self.budget > RESPONSE_BYTES {
            return Err(Error::Limit("Claude Code response exceeds 128 MiB".into()));
        }
        Ok((frame, size))
    }

    pub(super) async fn next(&mut self) -> Result<(Frame, usize), Error> {
        if let Some(frame) = self.queued.pop_front() {
            return Ok(frame);
        }
        let (mut frame, size) = self.read_frame().await?;
        let parsed = match frame["type"].as_str() {
            Some("stream_event") if frame["parent_tool_use_id"].is_null() => {
                Frame::Stream(frame["event"].take())
            }
            Some("assistant") if frame["parent_tool_use_id"].is_null() => Frame::Assistant(frame),
            Some("result") => Frame::Result(frame),
            Some("control_response") => Frame::Control(frame),
            Some("control_request") => self.incoming(&frame).await?,
            Some("rate_limit_event") => {
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
            Some("system") if frame["subtype"] == "compact_boundary" => {
                return Err(context_changed());
            }
            Some("conversation_reset") => return Err(context_changed()),
            _ => Frame::Quiet,
        };
        Ok((parsed, size))
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
        let mut received = vec![false; ids.len()];
        let wait = async {
            let mut remaining = ids.len();
            let mut queued = VecDeque::new();
            while remaining > 0 {
                let (frame, size) = self.next().await?;
                if let Frame::Control(frame) = frame {
                    let response = &frame["response"];
                    let Some(index) = ids
                        .iter()
                        .position(|id| response["request_id"] == id.as_str())
                    else {
                        continue;
                    };
                    if received[index] {
                        continue;
                    }
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
                    received[index] = true;
                    remaining -= 1;
                } else if !matches!(frame, Frame::Quiet) {
                    queued.push_back((frame, size));
                }
            }
            queued.append(&mut self.queued);
            self.queued = queued;
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
        *self.cancelled.lock().expect(crate::NOT_POISONED) = Cancellations::default();
        self.issued.clear();
        self.queued.clear();
        Ok(())
    }

    async fn incoming(&mut self, frame: &Value) -> Result<Frame, Error> {
        let outer = frame["request_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| failure("missing control request ID"))?;
        let request = &frame["request"];
        if request["subtype"] != "mcp_message" {
            self.send(&json!({"type":"control_response","response":{"subtype":"error","request_id":outer,"error":"Unsupported control request; no permission was granted"}})).await?;
            return Ok(Frame::Quiet);
        }
        let rpc = &request["message"];
        let decoded = serde_json::from_value::<JsonRpcMessage<Rpc, Value, Rpc>>(rpc.clone());
        let response = if request["server_name"] != "bart" {
            json!({"jsonrpc":"2.0","id":rpc["id"],"error":{"code":-32601,"message":"MCP server not found"}})
        } else if let Err(error) = &decoded {
            let id = rpc.get("id").filter(|id| {
                serde_json::from_value::<rmcp::model::RequestId>((*id).clone()).is_ok()
            });
            let code = if rpc.get("id").is_some() && id.is_none() {
                -32600
            } else {
                -32603
            };
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":error.to_string()}})
        } else if let Ok(JsonRpcMessage::Request(message)) = decoded {
            let result: Result<Value, Value> = match message.request.method.as_str() {
                "initialize" => params::<InitializeRequestParams>(&message.request).map(|params| {
                    let supported = ProtocolVersion::known_up_to(&ProtocolVersion::LATEST_WITH_INITIALIZE);
                    let version = if supported.contains(&params.protocol_version) {
                        params.protocol_version
                    } else {
                        ProtocolVersion::LATEST_WITH_INITIALIZE
                    };
                    json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"bart","version":env!("CARGO_PKG_VERSION")}})
                }),
                "tools/list" => params::<Option<PaginatedRequestParams>>(&message.request).map(|_| json!({"tools":self.tools})),
                "ping" => Ok(json!({})),
                "tools/call" => match params::<CallToolRequestParams>(&message.request) {
                    Err(error) => Err(error),
                    Ok(params) => {
                        if self
                            .cancelled
                            .lock()
                            .expect(crate::NOT_POISONED)
                            .contains(outer, &rpc["id"])
                        {
                            self.cancelled_reply(outer, &rpc["id"]).await?;
                            return Ok(Frame::Quiet);
                        }
                        let name = params.name.as_ref();
                        if self.tools.iter().any(|tool| tool["name"] == name) {
                            let id = params.meta.as_ref().and_then(|meta| meta.get("claudecode/toolUseId"))
                                .and_then(Value::as_str)
                                .filter(|id| !id.is_empty())
                                .unwrap_or(outer);
                            if self.pending.len() >= MAX_PENDING || self.issued.len() >= MAX_PENDING {
                                return Err(failure("too many Claude Code tool requests"));
                            }
                            if self
                                .pending
                                .iter()
                                .any(|pending| pending.outer == outer || pending.inner == rpc["id"])
                                || !self.issued.insert(id.to_owned())
                            {
                                return Err(failure("duplicate Claude Code tool request"));
                            }
                            let call = ToolCall {
                                id: id.to_owned(),
                                name: name.to_owned(),
                                arguments: Value::Object(params.arguments.unwrap_or_default()).into(),
                            };
                            let cancellation = CancellationToken::new();
                            {
                                let mut cancelled = self.cancelled.lock().expect(crate::NOT_POISONED);
                                if cancelled.contains(outer, &rpc["id"]) {
                                    cancellation.cancel();
                                }
                                cancelled
                                    .controls
                                    .insert(outer.to_owned(), cancellation.clone());
                                cancelled
                                    .requests
                                    .insert(rpc["id"].to_string(), cancellation.clone());
                            }
                            self.pending.push(Pending {
                                call: call.clone(),
                                outer: outer.to_owned(),
                                inner: rpc["id"].clone(),
                                cancellation,
                            });
                            return Ok(Frame::Call(call));
                        }
                        Ok(tool_error(&format!("Tool '{name}' not found")))
                    }
                },
                _ => Err(json!({"code":-32601,"message":"Method not found"})),
            };
            match result {
                Ok(result) => json!({"jsonrpc":"2.0","id":rpc["id"],"result":result}),
                Err(error) => json!({"jsonrpc":"2.0","id":rpc["id"],"error":error}),
            }
        } else {
            json!({"jsonrpc":"2.0","result":{}})
        };
        self.reply(&frame["request_id"], response).await?;
        Ok(Frame::Quiet)
    }

    async fn cancelled_reply(&mut self, outer: &str, inner: &Value) -> Result<(), Error> {
        let reply = {
            let cancelled = self.cancelled.lock().expect(crate::NOT_POISONED);
            !cancelled.control.contains(outer) && cancelled.rpc.contains(&inner.to_string())
        };
        if reply {
            self.reply(&json!(outer), json!({"jsonrpc":"2.0","id":inner,"error":{"code":-32800,"message":"Request cancelled"}})).await?;
        }
        Ok(())
    }

    async fn reply(&mut self, outer: &Value, response: Value) -> Result<(), Error> {
        self.send(&json!({"type":"control_response","response":{"subtype":"success","request_id":outer,"response":{"mcp_response":response}}})).await.map(drop)
    }

    pub(super) fn is_pending(&self, id: &str) -> bool {
        let cancelled = self.cancelled.lock().expect(crate::NOT_POISONED);
        self.pending
            .iter()
            .any(|pending| pending.call.id == id && !cancelled.control.contains(&pending.outer))
    }

    pub(super) fn finish_turn(&mut self) -> Result<(), Error> {
        if self.has_pending() {
            return Err(failure("Claude Code completed with unresolved tools"));
        }
        self.pending.clear();
        self.issued.clear();
        *self.cancelled.lock().expect(crate::NOT_POISONED) = Cancellations::default();
        Ok(())
    }

    fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| self.is_pending(&pending.call.id))
    }

    pub(super) fn admit(&self, call: &ToolCall) -> Result<CancellationToken, Error> {
        self.pending
            .iter()
            .find(|pending| &pending.call == call)
            .map(|pending| pending.cancellation.clone())
            .ok_or_else(|| failure("Claude Code tool request is no longer pending"))
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
            .position(|pending| pending.call.id == result.call_id)
            .ok_or_else(|| failure("Claude Code cancelled a tool callback"))?;
        let pending = self.pending.remove(index);
        let cancellation = pending.cancellation.clone();
        if cancellation.is_cancelled() {
            return self.cancelled_reply(&pending.outer, &pending.inner).await;
        }
        let output = result.output.clone();
        let is_error = result.is_error;
        let outer = pending.outer.clone();
        let inner = pending.inner.clone();
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
        if cancellation.is_cancelled() {
            return self.cancelled_reply(&outer, &inner).await;
        }
        self.send_line(line).await.map(drop)
    }

    pub(super) async fn stop(mut self) {
        kill_group(&self.child);
        let _ = self.child.wait().await;
    }
}

pub(super) fn kill_group(child: &Child) {
    crate::supervise::kill_group(child.id());
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.reader.abort();
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
