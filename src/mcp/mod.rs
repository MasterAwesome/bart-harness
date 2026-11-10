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
        StreamableHttpClientTransport,
        streamable_http_client::{StreamableHttpClientTransportConfig, StreamableHttpError},
    },
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, ReadBuf},
    process::{ChildStdin, ChildStdout},
    sync::{Semaphore, watch},
    task::AbortHandle,
    time,
};

use crate::{auth::Store, tool::Place};

mod command;
mod config;
#[cfg(test)]
mod http;
pub(crate) mod oauth;

pub(crate) use command::{Command, command, toggle};
pub(crate) use config::{Config, path};

const START_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TOOL_PAGES: usize = 100;
const MAX_TOOLS: usize = 10_000;
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
const MAX_REGISTRY_CATALOG_BYTES: usize = 64 * 1024 * 1024;
const MAX_STARTUPS: usize = 4;
const MAX_LINE_BYTES: usize = rmcp::transport::async_rw::DEFAULT_MAX_LINE_LENGTH;
const STDERR_TAIL_BYTES: usize = 4096;
const MAX_LISTED: usize = 200;
const MAX_GUIDANCE_BYTES: usize = 4096;
const MAX_NOTE_FIELD_CHARS: usize = 200;
const INHERITED: [&str; 10] = [
    "HOME", "LOGNAME", "PATH", "SHELL", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR", "TZ",
];

#[derive(Default)]
pub(crate) struct Registry {
    servers: Mutex<Vec<Server>>,
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
    task: Option<AbortHandle>,
}

#[derive(Clone)]
enum Status {
    Starting,
    Ready(Arc<Connection>),
    Failed(String),
    Disabled(String),
}

pub(crate) enum State {
    Starting,
    Ready { about: String, tool_count: usize },
    Failed(String),
    Disabled(String),
}

pub(crate) struct Catalog(Arc<CatalogData>);

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
    pub(crate) fn tools(&self) -> impl Iterator<Item = (&str, &str)> {
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

struct Connection {
    peer: Peer<RoleClient>,
    cancel: Mutex<Option<RunningServiceCancellationToken>>,
    catalog: Arc<CatalogData>,
    about: String,
    guidance: Option<String>,
    secrets: Vec<String>,
    child: Mutex<Option<crate::child::Owner>>,
    tail: Arc<Mutex<VecDeque<u8>>>,
}

impl Connection {
    fn stop(&self) {
        if let Some(cancel) = self.cancel.lock().expect(crate::NOT_POISONED).take() {
            cancel.cancel();
        }
        self.child.lock().expect(crate::NOT_POISONED).take();
    }
}

impl Server {
    fn stop(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
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
        } else if remote && self.place.confinement.network() == crate::tool::Network::Off {
            disabled(
                name,
                "remote servers are off: this session runs with --no-network",
            )
        } else {
            self.start(name, entry)
        }
    }

    fn start(&self, name: &str, entry: &config::Entry) -> Server {
        let (status, _) = watch::channel(Status::Starting);
        let report = status.clone();
        let task = tokio::spawn({
            let (name, entry) = (name.to_owned(), entry.clone());
            let (place, client, store) =
                (self.place.clone(), self.client.clone(), self.store.clone());
            let (budget, workers) = (Arc::clone(&self.budget), Arc::clone(&self.workers));
            let changes = self.changes.clone();
            async move {
                let worker = workers
                    .acquire()
                    .await
                    .expect("startup semaphore stays open");
                let resolved = config::resolve(&entry);
                let secrets = secrets(
                    &name,
                    resolved
                        .as_ref()
                        .ok()
                        .or_else(|| entry.settings().ok().map(|settings| &settings.transport)),
                    &store,
                );
                let tail = Arc::new(Mutex::new(VecDeque::new()));
                let connecting = async {
                    connect(
                        &name,
                        resolved?,
                        &place,
                        &client,
                        &store,
                        budget,
                        Arc::clone(&tail),
                    )
                    .await
                };
                let mut service = None;
                let status = match time::timeout(START_TIMEOUT, connecting).await {
                    Ok(Ok((mut connection, running))) => {
                        connection.secrets = secrets;
                        service = Some(running);
                        Status::Ready(Arc::new(connection))
                    }
                    Ok(Err(error)) => {
                        Status::Failed(clean(&with_tail(&error, &tail, &secrets), &secrets))
                    }
                    Err(_) => {
                        let error = with_tail(
                            &format!("startup did not finish within {}s", START_TIMEOUT.as_secs()),
                            &tail,
                            &secrets,
                        );
                        Status::Failed(clean(&error, &secrets))
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
                if let Some(service) = service {
                    let _ = service.waiting().await;
                    if report.send_if_modified(lost) {
                        changes.send_replace(());
                    }
                }
            }
        })
        .abort_handle();
        Server {
            name: name.to_owned(),
            status,
            task: Some(task),
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
        task: None,
    }
}

const USER_DISABLED: &str = "disabled by the user; /mcps enables it";

impl Registry {
    pub(crate) fn stop(&self) {
        for server in self.servers.lock().expect(crate::NOT_POISONED).drain(..) {
            server.stop();
        }
        self.changes.send_replace(());
    }

    pub(crate) fn start(
        config: &Config,
        place: Place,
        client: &Client,
        store: &Store,
    ) -> Arc<Self> {
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
            starter: Some(starter),
            discovery: Some(time::Instant::now() + START_TIMEOUT),
            changes,
        })
    }

    pub(crate) fn changes(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.statuses()
            .into_iter()
            .filter(|(_, status)| !matches!(*status.borrow(), Status::Disabled(_)))
            .map(|(name, _)| name)
            .collect()
    }

    pub(crate) fn states(&self) -> Vec<(String, State)> {
        self.statuses()
            .into_iter()
            .map(|(name, status)| {
                let state = match &*status.borrow() {
                    Status::Starting => State::Starting,
                    Status::Ready(connection) => State::Ready {
                        about: crate::provider::printable(
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

    pub(crate) fn is_empty(&self) -> bool {
        self.servers.lock().expect(crate::NOT_POISONED).is_empty()
    }

    pub(crate) fn catalog(&self, name: &str) -> Option<Catalog> {
        let servers = self.servers.lock().expect(crate::NOT_POISONED);
        let server = servers.iter().find(|server| server.name == name)?;
        match &*server.status.borrow() {
            Status::Ready(connection) => Some(Catalog(Arc::clone(&connection.catalog))),
            _ => None,
        }
    }

    fn apply(&self, name: &str, entry: &config::Entry) {
        let Some(starter) = &self.starter else { return };
        let mut servers = self.servers.lock().expect(crate::NOT_POISONED);
        let Some(server) = servers.iter_mut().find(|server| server.name == name) else {
            return;
        };
        server.stop();
        *server = starter.server(name, entry);
        self.changes.send_replace(());
    }

    fn statuses(&self) -> Vec<(String, watch::Receiver<Status>)> {
        self.servers
            .lock()
            .expect(crate::NOT_POISONED)
            .iter()
            .map(|server| (server.name.clone(), server.status.subscribe()))
            .collect()
    }

    pub(crate) async fn report_failures(&self) {
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

    pub(crate) fn servers(&self) -> Value {
        let records = self.states().into_iter().map(|(name, state)| match state {
            State::Starting => json!({"name": name, "status": "starting"}),
            State::Ready { about, tool_count } => {
                json!({"name": name, "status": "ready", "tool_count": tool_count, "about": about})
            }
            State::Failed(error) => json!({
                "name": name,
                "status": "failed",
                "error": crate::provider::printable(first_line(&error).unwrap_or_default(), &[]),
            }),
            State::Disabled(reason) => {
                json!({"name": name, "status": "disabled", "reason": reason})
            }
        });
        Value::Array(records.collect())
    }

    pub(crate) async fn tools(
        &self,
        server: Option<&str>,
        query: Option<&str>,
    ) -> Result<String, String> {
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
        let shown = lines.len().min(MAX_LISTED);
        let mut text: Vec<String> = lines.drain(..shown).map(|(_, line)| line).collect();
        if !lines.is_empty() {
            text.push(format!("({} more; narrow with `query`)", lines.len()));
        }
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

    pub(crate) async fn describe(&self, server: &str, tool: &str) -> Result<String, String> {
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
            let cut = guidance.floor_char_boundary(guidance.len().min(MAX_GUIDANCE_BYTES));
            let more = if cut < guidance.len() {
                concat!("\n", bart!(const Mcp, "guidance truncated"))
            } else {
                ""
            };
            let _ = write!(
                text,
                "\nserver-provided guidance:\n{}{more}",
                &guidance[..cut]
            );
        }
        Ok(text)
    }

    pub(crate) async fn call(
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

pub(crate) struct Outcome {
    pub(crate) value: Value,
    pub(crate) error: Option<String>,
    pub(crate) notes: Vec<String>,
    pub(crate) images: Vec<(String, String)>,
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
    tail: Arc<Mutex<VecDeque<u8>>>,
) -> Result<(Connection, RunningService<RoleClient, ClientConfig>), String> {
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    };
    let mut child = None;
    let start = async {
        match transport {
            config::Server::Stdio { command, args, env } => {
                let (started, transport) = stdio(place, &command, &args, env, &tail).await?;
                child = Some(started);
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
                let served = match oauth::client(name, &url, client, store).await? {
                    Some(authorized) => {
                        client_config()
                            .serve_with_lifecycle(
                                StreamableHttpClientTransport::with_client(authorized, config),
                                lifecycle,
                            )
                            .await
                    }
                    None => {
                        client_config()
                            .serve_with_lifecycle(
                                StreamableHttpClientTransport::with_client(client.clone(), config),
                                lifecycle,
                            )
                            .await
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
    let service = start.await?;
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
        child: Mutex::new(child),
        tail,
    };
    Ok((connection, service))
}

type Pipes = (Lines<ChildStdout>, ChildStdin);

async fn stdio(
    place: &Place,
    command: &str,
    args: &[String],
    env: BTreeMap<String, String>,
    tail: &Arc<Mutex<VecDeque<u8>>>,
) -> Result<(crate::child::Owner, Pipes), String> {
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
    let mut program = crate::tool::launch(command, place, &env)?;
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
    let mut child = crate::child::Owner::spawn(&mut program)
        .await
        .map_err(|error| format!("cannot run {command}: {error}"))?;
    let (Some(stdin), Some(stdout), Some(mut stderr)) = child.pipes() else {
        unreachable!("all three were piped");
    };
    // Keep enough extra bytes to redact secrets that cross the start of the tail.
    let keep = STDERR_TAIL_BYTES + env.values().map(String::len).max().unwrap_or(0);
    let tail = Arc::clone(tail);
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
        child,
        (
            Lines {
                inner: stdout,
                line: 0,
            },
            stdin,
        ),
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
    let redacted = crate::provider::redact(error, &secrets);
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
