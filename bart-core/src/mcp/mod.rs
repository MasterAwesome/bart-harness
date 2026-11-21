use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Write as _,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::{
    Client,
    header::{HeaderName, HeaderValue},
};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt as _, RoleClient,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
        PaginatedRequestParams, ProtocolVersion, ResourceContents, Tool,
    },
    service::{ClientInitializeError, Peer, RunningService, RunningServiceCancellationToken},
    transport::{
        StreamableHttpClientTransport, Transport,
        streamable_http_client::{StreamableHttpClientTransportConfig, StreamableHttpError},
    },
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, ReadBuf},
    process::{ChildStdin, ChildStdout},
    sync::{Semaphore, watch},
    time,
};

use crate::{
    auth::Store,
    confinement::{Descendants, Network, Place},
};

mod command;
mod config;
#[cfg(test)]
mod http;
pub(crate) mod oauth;

pub use command::{Add, Command, command, toggle};
pub use config::{Config, path};

const START_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TOOL_PAGES: usize = 100;
const MAX_TOOLS: usize = 10_000;
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
const MAX_REGISTRY_CATALOG_BYTES: usize = 64 * 1024 * 1024;
const MAX_STARTUPS: usize = 4;
const MAX_LINE_BYTES: usize = rmcp::transport::async_rw::DEFAULT_MAX_LINE_LENGTH;
const STDERR_TAIL_BYTES: usize = 4096;
const MAX_NOTE_FIELD_CHARS: usize = 200;
const INHERITED: [&str; 10] = [
    "HOME", "LOGNAME", "PATH", "SHELL", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR", "TZ",
];

#[derive(Default)]
pub struct Registry {
    servers: Mutex<Vec<Server>>,
    stopping: Mutex<Vec<Settlement>>,
    starter: Option<Starter>,
    discovery: Option<time::Instant>,
    changes: watch::Sender<()>,
}

struct Starter {
    place: Place,
    client: Client,
    store: Store,
    budget: Arc<Mutex<usize>>,
    workers: Arc<Semaphore>,
    changes: watch::Sender<()>,
}

struct Server {
    name: String,
    status: watch::Sender<Status>,
    cancel: tokio_util::sync::CancellationToken,
    settled: Option<Settlement>,
}

#[derive(Clone)]
enum Status {
    Starting,
    Ready(Arc<Connection>),
    Failed(String),
    Disabled(String),
}

pub enum State {
    Starting,
    Ready { about: String, tool_count: usize },
    Failed(String),
    Disabled(String),
}

pub struct Catalog(Arc<CatalogData>);

struct CatalogData {
    tools: Vec<Tool>,
    bytes: usize,
    budget: Arc<Mutex<usize>>,
}

impl Drop for CatalogData {
    fn drop(&mut self) {
        *self.budget.lock().expect(crate::NOT_POISONED) -= self.bytes;
    }
}

impl Catalog {
    pub fn tools(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.tools.iter().map(|tool| {
            (
                tool.name.as_ref(),
                tool.description
                    .as_deref()
                    .and_then(first_line)
                    .unwrap_or_default(),
            )
        })
    }
}

#[derive(Clone)]
struct Settlement {
    result: watch::Receiver<Option<Result<(), String>>>,
    http_server: Option<String>,
}

async fn settlement(mut settled: Settlement) -> Result<(), String> {
    let result = settled.result.wait_for(Option::is_some);
    // Bound the caller's wait, not the HTTP worker join. A retry can observe
    // completion even if an earlier caller timed out with the POST outstanding.
    let outcome = if let Some(name) = settled.http_server {
        time::timeout(crate::supervise::CLEANUP, result)
            .await
            .map_err(|_| {
                format!("MCP {name} HTTP shutdown unconfirmed: cleanup is still pending")
            })?
    } else {
        result.await
    };
    outcome
        .map_err(|_| "MCP cleanup worker ended without confirming settlement".to_owned())?
        .as_ref()
        .unwrap()
        .clone()
}

struct ServerIo {
    service: Option<RunningService<RoleClient, ClientConfig>>,
    http_cleanup: Option<HttpCleanup>,
    cancel: tokio_util::sync::CancellationToken,
    child: Option<crate::child::Owner>,
    tail: Arc<Mutex<VecDeque<u8>>>,
}

impl ServerIo {
    fn new(cancel: tokio_util::sync::CancellationToken) -> Self {
        Self {
            cancel,
            child: None,
            service: None,
            http_cleanup: None,
            tail: Default::default(),
        }
    }
}

// rmcp takes the transport by value. Until a RunningService exists, a failed
// or cancelled handshake drops it without calling close() to join its worker.
// Lend it exclusively for the handshake, retaining the same transport for close.
struct HttpLease<T>(tokio::sync::OwnedMutexGuard<T>);

impl<T: Transport<RoleClient>> Transport<RoleClient> for HttpLease<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        message: rmcp::model::ClientJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.0.send(message)
    }
    async fn receive(&mut self) -> Option<rmcp::model::ServerJsonRpcMessage> {
        self.0.receive().await
    }
    async fn close(&mut self) -> Result<(), Self::Error> {
        self.0.close().await
    }
}

type HttpCleanup = futures_util::future::BoxFuture<'static, bool>;

async fn serve_http(
    client: impl rmcp::transport::streamable_http_client::StreamableHttpClient,
    config: StreamableHttpClientTransportConfig,
    lifecycle: ClientLifecycleMode,
    cleanup: &mut Option<HttpCleanup>,
) -> Result<RunningService<RoleClient, ClientConfig>, Box<ClientInitializeError>> {
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let owner = Arc::new(tokio::sync::Mutex::new(transport));
    let lease = owner.clone().try_lock_owned().expect("new HTTP transport");
    *cleanup = Some(Box::pin(async move {
        owner.lock_owned().await.close().await.is_ok()
    }));
    let service = client_config()
        .serve_with_lifecycle(HttpLease(lease), lifecycle)
        .await?;
    // The service now owns and closes the transport on orderly shutdown.
    *cleanup = None;
    Ok(service)
}

impl Drop for ServerIo {
    fn drop(&mut self) {
        // An aborted lifecycle worker closes its settlement channel without a
        // confirmation. Its last-resort monitor reap must not block the executor.
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && let Some(child) = self.child.take()
        {
            handle.spawn_blocking(move || drop(child));
        }
        // Without an entered runtime the field drops synchronously.
    }
}

struct Connection {
    peer: Peer<RoleClient>,
    cancel: Mutex<Option<RunningServiceCancellationToken>>,
    catalog: Arc<CatalogData>,
    about: String,
    guidance: Option<String>,
    secrets: Vec<String>,
    tail: Arc<Mutex<VecDeque<u8>>>,
}

impl Connection {
    fn stop(&self) {
        if let Some(cancel) = self.cancel.lock().expect(crate::NOT_POISONED).take() {
            cancel.cancel();
        }
    }
}

impl Server {
    fn stop(&self) {
        self.cancel.cancel();
        if let Status::Ready(connection) = self
            .status
            .send_replace(Status::Failed("session stopped".into()))
        {
            connection.stop();
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Starter {
    fn server(&self, name: &str, entry: &config::Entry) -> Server {
        let remote = entry
            .settings()
            .is_ok_and(|settings| matches!(settings.transport, config::Server::Http { .. }));
        if entry.disabled() {
            disabled(name, USER_DISABLED)
        } else if remote && self.place.confinement.network() == Network::Off {
            disabled(
                name,
                "remote servers are off: this session runs with --no-network",
            )
        } else {
            self.start(name, entry, remote)
        }
    }

    fn start(&self, name: &str, entry: &config::Entry, http: bool) -> Server {
        let (status, _) = watch::channel(Status::Starting);
        let report = status.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (finished, settled) = watch::channel(None);
        tokio::spawn({
            let cancel = cancel.clone();
            let (name, entry) = (name.to_owned(), entry.clone());
            let (place, client, store) =
                (self.place.clone(), self.client.clone(), self.store.clone());
            let (budget, workers) = (Arc::clone(&self.budget), Arc::clone(&self.workers));
            let changes = self.changes.clone();
            async move {
                let mut io = ServerIo::new(cancel.clone());
                let start = async {
                    let worker = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return,
                        permit = workers.acquire() => permit.expect("startup semaphore stays open"),
                    };
                    let resolved = config::resolve(&entry);
                    let secrets = secrets(
                        &name,
                        resolved
                            .as_ref()
                            .ok()
                            .or_else(|| entry.settings().ok().map(|settings| &settings.transport)),
                        &store,
                    );
                    let tail = io.tail.clone();
                    let connecting = async {
                        connect(&name, resolved?, &place, &client, &store, budget, &mut io).await
                    };
                    let status = match connecting.await {
                        Ok(mut connection) => {
                            connection.secrets = secrets;
                            Status::Ready(Arc::new(connection))
                        }
                        Err(error) => {
                            Status::Failed(clean(&with_tail(&error, &tail, &secrets), &secrets))
                        }
                    };
                    drop(worker);
                    if report.send_if_modified(|current| {
                        if !matches!(current, Status::Starting) {
                            return false;
                        }
                        *current = status;
                        true
                    }) {
                        changes.send_replace(());
                    }
                };
                start.await;
                let mut service_result = Ok(());
                if let Some(service) = io.service.take() {
                    let shutdown = service.cancellation_token();
                    let waiting = service.waiting();
                    tokio::pin!(waiting);
                    let joined = if matches!(*report.borrow(), Status::Ready(_)) {
                        tokio::select! { result = &mut waiting => Some(result), () = cancel.cancelled() => None }
                    } else {
                        None
                    };
                    let joined = match joined {
                        Some(joined) => Some(joined),
                        None => {
                            shutdown.cancel();
                            if http {
                                Some(waiting.await)
                            } else {
                                time::timeout(crate::supervise::CLEANUP, waiting).await.ok()
                            }
                        }
                    };
                    service_result = match joined {
                        Some(joined) => joined
                            .map(drop)
                            .map_err(|e| format!("MCP {name} service failed: {e}")),
                        None => Err(format!("MCP {name} service shutdown unconfirmed")),
                    };
                    if report.send_if_modified(lost) {
                        changes.send_replace(());
                    }
                } else if let Some(close) = io.http_cleanup.take()
                    && !close.await
                {
                    service_result = Err(format!("MCP {name} HTTP worker shutdown unconfirmed"));
                }
                let result = if let Some(child) = io.child.take() {
                    child
                        .settle()
                        .await
                        .map_err(|error| format!("MCP {name} cleanup unconfirmed: {error}"))
                } else {
                    Ok(())
                };
                finished.send_replace(Some(result.and(service_result)));
            }
        });
        Server {
            name: name.to_owned(),
            status,
            cancel,
            settled: Some(Settlement {
                result: settled,
                http_server: http.then(|| name.to_owned()),
            }),
        }
    }
}

fn lost(status: &mut Status) -> bool {
    let Status::Ready(connection) = status else {
        return false;
    };
    connection.stop();
    let lost = "the connection closed (the server exited or dropped it); disable and re-enable it in /mcps to reconnect";
    *status = Status::Failed(clean(
        &with_tail(lost, &connection.tail, &connection.secrets),
        &connection.secrets,
    ));
    true
}

fn disabled(name: &str, reason: &str) -> Server {
    Server {
        name: name.to_owned(),
        status: watch::channel(Status::Disabled(reason.to_owned())).0,
        cancel: Default::default(),
        settled: None,
    }
}

const USER_DISABLED: &str = "disabled by the user; /mcps enables it";

impl Registry {
    pub fn stop(&self) {
        let mut stopping = self.stopping.lock().expect(crate::NOT_POISONED);
        let servers = std::mem::take(&mut *self.servers.lock().expect(crate::NOT_POISONED));
        for server in servers {
            server.stop();
            stopping.extend(server.settled);
        }
        self.changes.send_replace(());
    }

    pub async fn close(&self) -> Result<(), String> {
        self.stop();
        let stopping = self.stopping.lock().expect(crate::NOT_POISONED).clone();
        let results = futures_util::future::join_all(stopping.into_iter().map(settlement)).await;
        results.into_iter().collect()
    }

    pub fn start(config: &Config, place: Place, client: &Client, store: &Store) -> Arc<Self> {
        let changes = watch::channel(()).0;
        let starter = Starter {
            place,
            client: client.clone(),
            store: store.clone(),
            budget: Arc::default(),
            workers: Arc::new(Semaphore::new(MAX_STARTUPS)),
            changes: changes.clone(),
        };
        let servers = config
            .entries()
            .map(|(name, entry)| starter.server(name, entry))
            .collect();
        Arc::new(Self {
            servers: Mutex::new(servers),
            stopping: Default::default(),
            starter: Some(starter),
            discovery: Some(time::Instant::now() + START_TIMEOUT),
            changes,
        })
    }

    pub fn changes(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    pub fn names(&self) -> Vec<String> {
        self.statuses()
            .into_iter()
            .filter(|(_, status)| !matches!(*status.borrow(), Status::Disabled(_)))
            .map(|(name, _)| name)
            .collect()
    }

    pub fn states(&self) -> Vec<(String, State)> {
        self.statuses()
            .into_iter()
            .map(|(name, status)| {
                let state = match &*status.borrow() {
                    Status::Starting => State::Starting,
                    Status::Ready(connection) => State::Ready {
                        about: crate::terminal_text::printable(
                            first_line(&connection.about).unwrap_or_default(),
                            &[],
                        ),
                        tool_count: connection.catalog.tools.len(),
                    },
                    Status::Failed(error) => State::Failed(error.clone()),
                    Status::Disabled(reason) => State::Disabled(reason.clone()),
                };
                (name, state)
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.lock().expect(crate::NOT_POISONED).is_empty()
    }

    pub fn catalog(&self, name: &str) -> Option<Catalog> {
        let servers = self.servers.lock().expect(crate::NOT_POISONED);
        let server = servers.iter().find(|server| server.name == name)?;
        match &*server.status.borrow() {
            Status::Ready(connection) => Some(Catalog(Arc::clone(&connection.catalog))),
            _ => None,
        }
    }

    async fn apply(&self, name: &str, entry: &config::Entry) -> Result<(), String> {
        let Some(starter) = &self.starter else {
            return Ok(());
        };
        let (status, settled) = {
            let servers = self.servers.lock().expect(crate::NOT_POISONED);
            let Some(server) = servers.iter().find(|s| s.name == name) else {
                return Ok(());
            };
            server.stop();
            self.changes.send_replace(());
            (server.status.clone(), server.settled.clone())
        };
        if let Some(settled) = settled {
            settlement(settled).await?;
        }
        // The permit to replace belongs to the server we actually stopped.
        let mut servers = self.servers.lock().expect(crate::NOT_POISONED);
        if let Some(server) = servers.iter_mut().find(|s| s.name == name) {
            if !server.status.same_channel(&status) {
                return Err(format!("MCP {name} already replaced"));
            }
            *server = starter.server(name, entry);
            self.changes.send_replace(());
        }
        Ok(())
    }

    fn statuses(&self) -> Vec<(String, watch::Receiver<Status>)> {
        self.servers
            .lock()
            .expect(crate::NOT_POISONED)
            .iter()
            .map(|server| (server.name.clone(), server.status.subscribe()))
            .collect()
    }

    pub async fn report_failures(&self) {
        futures_util::future::join_all(self.statuses().into_iter().map(
            |(name, mut status)| async move {
                loop {
                    if let Status::Failed(error) = &*status.borrow() {
                        crate::terminal_text::stderr_line(format_args!("MCP {name}: {error}"));
                        break;
                    }
                    if status.changed().await.is_err() {
                        break;
                    }
                }
            },
        ))
        .await;
    }

    pub fn servers(&self) -> Value {
        let records = self.states().into_iter().map(|(name, state)| match state {
            State::Starting => json!({"name": name, "status": "starting"}),
            State::Ready { about, tool_count } => {
                json!({"name": name, "status": "ready", "tool_count": tool_count, "about": about})
            }
            State::Failed(error) => json!({
                "name": name,
                "status": "failed",
                "error": crate::terminal_text::printable(first_line(&error).unwrap_or_default(), &[]),
            }),
            State::Disabled(reason) => {
                json!({"name": name, "status": "disabled", "reason": reason})
            }
        });
        Value::Array(records.collect())
    }

    pub async fn tools(&self, server: Option<&str>, query: Option<&str>) -> Result<String, String> {
        let words: Vec<String> = query
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut ready = Vec::new();
        let (mut starting, mut unavailable) = (Vec::new(), Vec::new());
        if let Some(name) = server {
            ready.push((name.to_owned(), self.connection(name).await?));
        } else {
            let deadline = self.discovery.unwrap_or_else(time::Instant::now);
            for (_, status) in self.statuses() {
                let _ = time::timeout_at(deadline, self::ready(status)).await;
            }
            for (name, status) in self.statuses() {
                match &*status.borrow() {
                    Status::Ready(connection) => ready.push((name, Arc::clone(connection))),
                    Status::Starting => starting.push(name),
                    Status::Failed(_) | Status::Disabled(_) => unavailable.push(name),
                }
            }
        }
        let mut lines = Vec::new();
        for (name, connection) in &ready {
            for tool in &connection.catalog.tools {
                let tool_name = tool.name.to_lowercase();
                let searched = searchable(tool);
                if words.iter().all(|word| {
                    tool_name.contains(word.as_str()) || searched.contains(word.as_str())
                }) {
                    let rank = words
                        .iter()
                        .filter(|word| tool_name.contains(word.as_str()))
                        .count();
                    let about = tool
                        .description
                        .as_deref()
                        .and_then(first_line)
                        .map_or_else(String::new, |about| format!(" — {about}"));
                    lines.push((
                        rank,
                        format!("server {name:?}, tool {:?}{about}", tool.name),
                    ));
                }
            }
        }
        lines.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
        let mut text: Vec<String> = lines.into_iter().map(|(_, line)| line).collect();
        if text.is_empty() {
            text.push("no matching tools".to_owned());
        }
        if !starting.is_empty() {
            text.push(format!(
                "still starting, not searched: {}",
                starting.join(", ")
            ));
        }
        if !unavailable.is_empty() {
            text.push(format!(
                "unavailable, not searched (mcp.servers() says why): {}",
                unavailable.join(", ")
            ));
        }
        Ok(text.join("\n"))
    }

    pub async fn describe(&self, server: &str, tool: &str) -> Result<String, String> {
        let connection = self.connection(server).await?;
        let tool = find(&connection, server, tool)?;
        let schema = serde_json::to_string_pretty(&*tool.input_schema).unwrap_or_default();
        let mut text = format!("server {server:?}, tool {:?}", tool.name);
        if let Some(description) = &tool.description {
            let _ = write!(text, "\n{description}");
        }
        let _ = write!(text, "\ninput schema: {schema}");
        if let Some(output) = &tool.output_schema {
            let _ = write!(
                text,
                "\noutput schema: {}",
                serde_json::to_string_pretty(&**output).unwrap_or_default()
            );
        }
        if let Some(guidance) = &connection.guidance {
            let _ = write!(text, "\nserver-provided guidance:\n{guidance}");
        }
        Ok(text)
    }

    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<Outcome, String> {
        let connection = self.connection(server).await?;
        find(&connection, server, tool)?;
        let secrets = || {
            let mut secrets = connection.secrets.clone();
            if let Some(starter) = &self.starter {
                secrets.extend(oauth::stored_secrets(server, &starter.store));
            }
            secrets
        };
        let result = connection
            .peer
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
            .await
            .map_err(|error| {
                clean(
                    &format!("server {server:?}, tool {tool:?} failed: {error}"),
                    &secrets(),
                )
            })?;
        let mut outcome = outcome(result);
        outcome.error = outcome.error.map(|text| {
            clean(
                &format!("server {server:?}, tool {tool:?}: {text}"),
                &secrets(),
            )
        });
        Ok(outcome)
    }

    async fn connection(&self, name: &str) -> Result<Arc<Connection>, String> {
        let (_, status) = self
            .statuses()
            .into_iter()
            .find(|(server, _)| server == name)
            .ok_or_else(|| format!("no MCP server named {name}; mcp.servers() lists them"))?;
        ready(status).await
    }
}

async fn ready(mut status: watch::Receiver<Status>) -> Result<Arc<Connection>, String> {
    let settled = status
        .wait_for(|status| !matches!(status, Status::Starting))
        .await
        .map_err(|_| "stopped while starting".to_owned())?;
    match &*settled {
        Status::Ready(connection) => Ok(Arc::clone(connection)),
        Status::Failed(error) => Err(error.clone()),
        Status::Disabled(reason) => Err(reason.clone()),
        Status::Starting => unreachable!("waited for a settled status"),
    }
}

fn find<'a>(connection: &'a Connection, server: &str, tool: &str) -> Result<&'a Tool, String> {
    connection
        .catalog
        .tools
        .iter()
        .find(|candidate| candidate.name == tool)
        .ok_or_else(|| format!("{server} has no tool {tool}; mcp.tools({server:?}) lists them"))
}

fn searchable(tool: &Tool) -> String {
    fn object(object: &Map<String, Value>, text: &mut String) {
        if let Some(Value::Object(found)) = object.get("properties") {
            for (name, property) in found {
                text.push('\n');
                text.push_str(&name.to_lowercase());
                if let Some(description) = property.get("description").and_then(Value::as_str) {
                    text.push('\n');
                    text.push_str(&description.to_lowercase());
                }
            }
        }
        object.values().for_each(|value| walk(value, text));
    }
    fn walk(value: &Value, text: &mut String) {
        match value {
            Value::Object(found) => object(found, text),
            Value::Array(items) => items.iter().for_each(|item| walk(item, text)),
            _ => {}
        }
    }
    let mut text = tool
        .description
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    object(&tool.input_schema, &mut text);
    text
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

pub struct Outcome {
    pub value: Value,
    pub error: Option<String>,
    pub notes: Vec<String>,
    pub images: Vec<(String, String)>,
}

fn outcome(result: rmcp::model::CallToolResult) -> Outcome {
    let structured = result.structured_content;
    let (mut texts, mut notes, mut images) = (Vec::new(), Vec::new(), Vec::new());
    for block in result.content {
        match block {
            ContentBlock::Text(text) => {
                let duplicate = structured.as_ref().is_some_and(|value| {
                    serde_json::from_str::<Value>(&text.text).is_ok_and(|parsed| &parsed == value)
                });
                if !duplicate {
                    texts.push(text.text);
                }
            }
            ContentBlock::Image(image) => images.push((image.mime_type, image.data)),
            ContentBlock::Resource(ref embedded)
                if let ResourceContents::TextResourceContents { text, .. } = &embedded.resource =>
            {
                texts.push(text.clone());
            }
            other => notes.push(unshown(&other)),
        }
    }
    let text = texts.join("\n");
    if result.is_error == Some(true) {
        return Outcome {
            value: Value::Null,
            error: Some(if text.is_empty() {
                structured.map_or_else(
                    || "tool reported an error".to_owned(),
                    |value| value.to_string(),
                )
            } else {
                text
            }),
            notes,
            images,
        };
    }
    let value = match structured {
        Some(value) => {
            if !text.is_empty() {
                notes.insert(0, text);
            }
            value
        }
        None => Value::String(text),
    };
    Outcome {
        value,
        error: None,
        notes,
        images,
    }
}

fn unshown(block: &ContentBlock) -> String {
    let block = serde_json::to_value(block).unwrap_or_default();
    let resource = block.get("resource").unwrap_or(&block);
    let kind = block["type"].as_str().unwrap_or("content");
    let detail = [&resource["uri"], &resource["mimeType"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(|field| {
            field
                .chars()
                .take(MAX_NOTE_FIELD_CHARS)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    bart!(Mcp, "{kind} {detail} not shown")
}

fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("bart", env!("CARGO_PKG_VERSION")),
    )
}

async fn connect(
    name: &str,
    transport: config::Server,
    place: &Place,
    client: &Client,
    store: &Store,
    budget: Arc<Mutex<usize>>,
    io: &mut ServerIo,
) -> Result<Connection, String> {
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    };
    // Once process creation starts, do not cancel its waiter before ownership transfers.
    let deadline = time::Instant::now() + START_TIMEOUT;
    let pipes = if let config::Server::Stdio { command, args, env } = &transport {
        if io.cancel.is_cancelled() {
            return Err("session stopped".into());
        }
        Some(stdio(place, command, args, env.clone(), io).await?)
    } else {
        None
    };
    let handshake = async {
        match transport {
            config::Server::Stdio { .. } => {
                let transport = pipes.expect("stdio transport");
                client_config()
                    .serve_with_lifecycle(transport, lifecycle)
                    .await
                    .map_err(|error| format!("handshake failed: {error}"))
            }
            config::Server::Http { url, headers } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                for (header, value) in headers {
                    let parsed = HeaderName::try_from(header.as_str())
                        .ok()
                        .zip(HeaderValue::try_from(value.as_str()).ok());
                    let (header, value) =
                        parsed.ok_or_else(|| format!("invalid header {header}"))?;
                    config.custom_headers.insert(header, value);
                }
                let authorized = oauth::client(name, &url, client, store).await?;
                let served = match authorized {
                    Some(authorized) => {
                        serve_http(authorized, config, lifecycle, &mut io.http_cleanup).await
                    }
                    None => {
                        serve_http(client.clone(), config, lifecycle, &mut io.http_cleanup).await
                    }
                };
                served.map_err(|error| {
                    if needs_login(&error) {
                        format!(
                            "{error}; needs sign-in: run `bart mcp login {}`",
                            crate::shell_quote::quote(name)
                        )
                    } else {
                        error.to_string()
                    }
                })
            }
        }
    };
    let discover = async {
        let service = io.service.insert(handshake.await?);
        let tools = list_tools(service.peer(), budget).await?;
        let info = service.peer_info();
        let guidance = info.as_ref().and_then(|info| info.instructions.clone());
        let about = guidance
            .clone()
            .or_else(|| {
                info.as_ref()
                    .and_then(|info| info.server_info.as_ref())
                    .and_then(|server| server.title.clone().or_else(|| server.description.clone()))
            })
            .unwrap_or_default();
        let connection = Connection {
            peer: service.peer().clone(),
            cancel: Mutex::new(Some(service.cancellation_token())),
            catalog: Arc::new(tools),
            about,
            guidance,
            secrets: Vec::new(),
            tail: Arc::clone(&io.tail),
        };
        Ok(connection)
    };
    tokio::select! {
        biased;
        () = io.cancel.cancelled() => Err("session stopped".into()),
        result = time::timeout_at(deadline, discover) => result.map_err(|_| format!("startup did not finish within {}s", START_TIMEOUT.as_secs()))?,
    }
}

type Pipes = (Lines<ChildStdout>, ChildStdin);

async fn stdio(
    place: &Place,
    command: &str,
    args: &[String],
    env: BTreeMap<String, String>,
    io: &mut ServerIo,
) -> Result<Pipes, String> {
    if let Some(name) = env.keys().find(|name| name.starts_with("BART_SUPERVISOR_")) {
        return Err(format!("{name} is reserved for Bart"));
    }
    let mut size = 0usize;
    for (name, value) in &env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err("invalid environment name or value".into());
        }
        size = size
            .saturating_add(name.len())
            .saturating_add(value.len())
            .saturating_add(11);
    }
    if size > 32 * 1024 {
        return Err("its environment exceeds 32 KiB".into());
    }
    let mut program =
        crate::confinement::launch(command, place, &env, &place.cwd, Descendants::Reap)?;
    if !place.confinement.jailed() {
        program
            .env_clear()
            .envs(
                INHERITED
                    .iter()
                    .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
            )
            .envs(&env);
    }
    program
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::child::Owner::spawn_into(&mut program, &mut io.child)
        .await
        .map_err(|error| {
            format!(
                "cannot run {command}: {}",
                crate::terminal_text::io_reason(&error)
            )
        })?;
    let child = io.child.as_mut().expect("spawned MCP monitor");
    let (Some(stdin), Some(stdout), Some(mut stderr)) = child.pipes() else {
        unreachable!("all three were piped");
    };
    // Keep enough extra bytes to redact secrets that cross the start of the tail.
    let keep = STDERR_TAIL_BYTES + env.values().map(String::len).max().unwrap_or(0);
    let tail = Arc::clone(&io.tail);
    child.task(tokio::spawn(async move {
        let mut buffer = [0; 4096];
        while let Ok(read @ 1..) = stderr.read(&mut buffer).await {
            let mut tail = tail.lock().expect(crate::NOT_POISONED);
            tail.extend(&buffer[..read]);
            let excess = tail.len().saturating_sub(keep);
            tail.drain(..excess);
        }
    }));
    Ok((
        Lines {
            inner: stdout,
            line: 0,
        },
        stdin,
    ))
}

// Fail closed so pending calls do not wait for replies discarded by rmcp.
struct Lines<R> {
    inner: R,
    line: usize,
}

impl<R: AsyncRead + Unpin> AsyncRead for Lines<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        std::task::ready!(std::pin::Pin::new(&mut self.inner).poll_read(context, buf))?;
        let mut line = self.line;
        for segment in buf.filled()[before..].split_inclusive(|&byte| byte == b'\n') {
            line += segment.len();
            if line > MAX_LINE_BYTES {
                buf.set_filled(before);
                return std::task::Poll::Ready(Err(std::io::Error::other(format!(
                    "a message exceeded {} MiB",
                    MAX_LINE_BYTES >> 20
                ))));
            }
            if segment.ends_with(b"\n") {
                line = 0;
            }
        }
        self.line = line;
        std::task::Poll::Ready(Ok(()))
    }
}

fn secrets(name: &str, server: Option<&config::Server>, store: &Store) -> Vec<String> {
    let (values, url): (Vec<(&String, &String)>, _) = match server {
        Some(config::Server::Stdio { env, .. }) => (env.iter().collect(), Vec::new()),
        Some(config::Server::Http { headers, url }) => (headers.iter().collect(), url_secrets(url)),
        None => (Vec::new(), Vec::new()),
    };
    values
        .into_iter()
        .filter(|(name, value)| {
            let name = name.to_ascii_lowercase();
            value.chars().count() >= MIN_URL_SECRET_CHARS
                || name == "authorization"
                || ["token", "secret", "password", "key"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
        })
        .flat_map(|(_, value)| {
            let token = value
                .trim_matches([' ', '\t'])
                .split_once(' ')
                .filter(|(scheme, _)| {
                    ["Bearer", "Basic", "Token"]
                        .iter()
                        .any(|known| scheme.eq_ignore_ascii_case(known))
                })
                .map(|(_, credential)| credential.trim_matches([' ', '\t']))
                .filter(|credential| !credential.is_empty());
            std::iter::once(value.clone()).chain(token.map(str::to_owned))
        })
        .chain(url)
        .chain(oauth::stored_secrets(name, store))
        .filter(|secret| !secret.is_empty())
        .collect()
}

// Redacting very short values would hide unrelated text.
const MIN_URL_SECRET_CHARS: usize = 4;

fn url_secrets(url: &str) -> Vec<String> {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return vec![url.to_owned()];
    };
    let raw = [parsed.username(), parsed.password().unwrap_or_default()]
        .into_iter()
        .chain(parsed.query())
        .chain(parsed.fragment())
        .chain(
            parsed
                .query()
                .into_iter()
                .flat_map(|query| query.split('&'))
                .filter_map(|pair| pair.split_once('=').map(|(_, value)| value)),
        );
    let mut secrets: Vec<String> = raw
        .flat_map(|text| [text.to_owned(), percent_decode(text)])
        .filter(|secret| secret.chars().count() >= MIN_URL_SECRET_CHARS)
        .collect();
    secrets.extend(
        parsed
            .password()
            .filter(|password| !password.is_empty())
            .into_iter()
            .flat_map(|password| [password.to_owned(), percent_decode(password)]),
    );
    secrets.extend(
        parsed
            .query_pairs()
            .map(|(_, value)| value.into_owned())
            .filter(|value| value.chars().count() >= MIN_URL_SECRET_CHARS),
    );
    secrets.sort();
    secrets.dedup();
    secrets
}

fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

fn clean(error: &str, secrets: &[String]) -> String {
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    let redacted = crate::terminal_text::redact(error, &secrets);
    let safe = redacted
        .lines()
        .take(64)
        .map(|line| {
            crate::terminal_text::safe_text(line, crate::terminal_text::TextMode::Diagnostic)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if safe.len() <= STDERR_TAIL_BYTES {
        return safe;
    }
    let marker = concat!("\n", bart!(const Mcp, "diagnostic truncated"), "\n");
    let head = safe.floor_char_boundary(STDERR_TAIL_BYTES / 2);
    let tail = safe.ceil_char_boundary(safe.len() - (STDERR_TAIL_BYTES - head - marker.len()));
    format!("{}{marker}{}", &safe[..head], &safe[tail..])
}

fn with_tail(error: &str, tail: &Mutex<VecDeque<u8>>, secrets: &[String]) -> String {
    let mut bytes: Vec<u8> = tail
        .lock()
        .expect(crate::NOT_POISONED)
        .iter()
        .copied()
        .collect();
    let mut covered = vec![false; bytes.len()];
    for secret in secrets
        .iter()
        .map(String::as_bytes)
        .filter(|secret| !secret.is_empty())
    {
        for (at, window) in bytes.windows(secret.len()).enumerate() {
            if window == secret {
                covered[at..at + secret.len()].fill(true);
            }
        }
    }
    for (byte, covered) in bytes.iter_mut().zip(covered) {
        if covered {
            *byte = b'*';
        }
    }
    let shown = &bytes[bytes.len().saturating_sub(STDERR_TAIL_BYTES)..];
    let text = String::from_utf8_lossy(shown).trim().to_owned();
    if text.is_empty() {
        error.to_owned()
    } else {
        format!("{error}; its stderr ends:\n{text}")
    }
}

fn needs_login(error: &ClientInitializeError) -> bool {
    let ClientInitializeError::TransportError { error, .. } = error else {
        return false;
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&*error.error);
    while let Some(error) = source {
        if let Some(
            StreamableHttpError::<reqwest::Error>::AuthRequired(_)
            | StreamableHttpError::<reqwest::Error>::InsufficientScope(_),
        ) = error.downcast_ref()
        {
            return true;
        }
        source = error.source();
    }
    false
}

async fn list_tools(
    service: &Peer<RoleClient>,
    budget: Arc<Mutex<usize>>,
) -> Result<CatalogData, String> {
    let mut tools = CatalogData {
        tools: Vec::new(),
        bytes: 0,
        budget,
    };
    let mut bytes = 0usize;
    let mut cursor = None;
    for _ in 0..MAX_TOOL_PAGES {
        let page = service
            .list_tools(Some(
                PaginatedRequestParams::default().with_cursor(cursor.clone()),
            ))
            .await
            .map_err(|error| format!("cannot list tools: {error}"))?;
        for tool in &page.tools {
            bytes = bytes.saturating_add(crate::history::encoded_len(tool));
            if bytes > MAX_CATALOG_BYTES {
                return Err(format!(
                    "tool catalog exceeds {} MiB",
                    MAX_CATALOG_BYTES / (1024 * 1024)
                ));
            }
        }
        if tools.tools.len().saturating_add(page.tools.len()) > MAX_TOOLS {
            return Err(format!("more than {MAX_TOOLS} tools"));
        }
        {
            let mut used = tools.budget.lock().expect(crate::NOT_POISONED);
            let page_bytes = bytes - tools.bytes;
            if used.saturating_add(page_bytes) > MAX_REGISTRY_CATALOG_BYTES {
                return Err(format!(
                    "registry tool catalogs exceed {} MiB",
                    MAX_REGISTRY_CATALOG_BYTES / (1024 * 1024)
                ));
            }
            *used += page_bytes;
            tools.bytes = bytes;
        }
        tools.tools.extend(page.tools);
        match page.next_cursor {
            Some(next) if cursor.as_ref() == Some(&next) => {
                return Err("tool list repeated its cursor".into());
            }
            Some(next) => cursor = Some(next),
            None => return Ok(tools),
        }
    }
    Err(format!("tool list longer than {MAX_TOOL_PAGES} pages"))
}

#[cfg(test)]
mod tests;
