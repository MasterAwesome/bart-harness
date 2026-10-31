use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use tokio::time;

use crate::{
    auth::{self, Credential, Store},
    history::{Assistant, Message},
    tool::Definition,
};

mod catalog;
mod chat;
mod chatgpt;
mod claude_code;
mod config;
mod error;
mod gemini;
mod http;
mod messages;
mod prepare;
mod responses;
pub(crate) mod search;
mod sse;
mod transport;

pub(crate) use catalog::{Model, Visibility, model, models};
use catalog::{Page, Paging, data_models};
pub(crate) use claude_code::available as claude_code_available;
pub(crate) use config::{CustomUpdate, all, is_builtin};

pub(crate) use error::{Error, Retry, printable, redact};
use error::{error_message, exhausted_reset, http_error, http_retry, quota_retry};
use http::{Http, authorize};
use transport::Route;
pub(crate) use transport::{Transport, stream};

const RESPONSE_BYTES: usize = 128 * 1024 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Provider {
    endpoint: Option<Box<Url>>,
    kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Chatgpt,
    Claude,
    Custom {
        name: String,
        protocol: Protocol,
        key: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Protocol {
    Responses,
    Chat,
    Messages,
    Gemini,
    Claude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Auth {
    Chatgpt,
    External,
    ApiKey,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Custom {
    pub(crate) protocol: Protocol,
    pub(crate) url: String,
    pub(crate) key: bool,
}

fn at(url: &Url, host: &str, base: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port_or_known_default() == Some(443)
        && url.path().trim_end_matches('/') == base
}

impl Provider {
    pub(crate) fn name(&self) -> &str {
        match &self.kind {
            Kind::Chatgpt => "chatgpt",
            Kind::Claude => "claude",
            Kind::Custom { name, .. } => name,
        }
    }

    pub(crate) fn label(&self) -> &str {
        if self.kind == Kind::Claude {
            "claude-code"
        } else {
            self.name()
        }
    }

    pub(crate) fn protocol(&self) -> Protocol {
        match self.kind {
            Kind::Chatgpt => Protocol::Responses,
            Kind::Claude => Protocol::Claude,
            Kind::Custom { protocol, .. } => protocol,
        }
    }

    pub(crate) fn auth(&self) -> Auth {
        match self.kind {
            Kind::Chatgpt => Auth::Chatgpt,
            Kind::Claude => Auth::External,
            Kind::Custom { key: true, .. } => Auth::ApiKey,
            Kind::Custom { key: false, .. } => Auth::None,
        }
    }

    pub(crate) fn endpoint(&self) -> Option<&Url> {
        self.endpoint.as_deref()
    }

    fn url(&self, path: &str) -> Result<Url, Error> {
        let mut url = self
            .endpoint()
            .cloned()
            .ok_or_else(|| Error::Invalid("this provider has no HTTP endpoint".into()))?;
        url.set_path(&format!("{}{path}", url.path().trim_end_matches('/')));
        Ok(url)
    }

    fn stream_url(&self, model: &str) -> Result<Url, Error> {
        let mut url = match self.protocol() {
            Protocol::Responses => self.url("/responses")?,
            Protocol::Chat => self.url("/chat/completions")?,
            Protocol::Messages => self.url("/messages")?,
            Protocol::Gemini => self.url(&gemini::path(model)?)?,
            Protocol::Claude => return Err(Error::Invalid("Claude uses a local process".into())),
        };
        if self.protocol() == Protocol::Gemini {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        Ok(url)
    }

    fn codex(&self) -> bool {
        self.kind == Kind::Chatgpt
    }

    pub(crate) fn capabilities(&self) -> Capabilities {
        let web_search = self.codex()
            || match self.protocol() {
                Protocol::Responses => self.endpoint().is_some_and(responses::openai_api),
                Protocol::Chat => self
                    .endpoint()
                    .is_some_and(|url| at(url, "openrouter.ai", "/api/v1")),
                Protocol::Messages => self
                    .endpoint()
                    .is_some_and(|url| at(url, "api.anthropic.com", "/v1")),
                Protocol::Gemini => self
                    .endpoint()
                    .is_some_and(|url| at(url, "generativelanguage.googleapis.com", "/v1beta")),
                Protocol::Claude => false,
            };
        Capabilities {
            output_limit: !self.codex(),
            web_search,
        }
    }

    fn builtin() -> Self {
        Self {
            endpoint: Some(Box::new(
                Url::parse("https://chatgpt.com/backend-api/codex")
                    .expect("built-in HTTP endpoint"),
            )),
            kind: Kind::Chatgpt,
        }
    }

    fn claude() -> Self {
        Self {
            endpoint: None,
            kind: Kind::Claude,
        }
    }

    pub(crate) fn custom(name: &str, custom: &Custom) -> Result<Self, String> {
        if name.is_empty()
            || name.starts_with('-')
            || name.contains(':')
            || name.chars().any(crate::terminal_text::unsafe_character)
        {
            return Err("a custom provider needs a nonempty, printable single-line name without ':' or a leading '-'".into());
        }
        if custom.protocol == Protocol::Claude {
            return Err("Claude is a local built-in provider, not an HTTP protocol".into());
        }
        if is_builtin(name) {
            return Err(format!(
                "{name} is a built-in provider; a custom provider needs another name"
            ));
        }
        let url = reqwest::Url::parse(&custom.url)
            .map_err(|_| "the provider URL must be an absolute HTTP(S) URL".to_owned())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "the provider URL must be HTTP(S), without credentials, query or fragment".into(),
            );
        }
        Ok(Self {
            endpoint: Some(Box::new(url)),
            kind: Kind::Custom {
                name: name.into(),
                protocol: custom.protocol,
                key: custom.key,
            },
        })
    }

    #[must_use]
    pub(crate) fn login_hint(&self) -> String {
        match self.auth() {
            Auth::Chatgpt => "bart login chatgpt".into(),
            Auth::External => "claude auth login".into(),
            _ => format!(
                "bart login {} < key-file",
                crate::shell_quote::quote(self.name())
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Capabilities {
    pub(crate) output_limit: bool,
    pub(crate) web_search: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Tools<'a> {
    Functions(&'a [Definition]),
    /// Keep the same definitions on the wire, but forbid new tool calls.
    Disabled(&'a [Definition]),
    WebSearch,
}

pub(crate) struct Request<'a> {
    pub(crate) provider: &'a Provider,
    pub(crate) model: &'a str,
    pub(crate) instructions: &'a str,
    pub(crate) history: &'a [Message],
    pub(crate) tools: Tools<'a>,
    pub(crate) reasoning_effort: Option<&'a str>,
    pub(crate) summaries: bool,
    pub(crate) verbosity: Option<&'a str>,
    pub(crate) max_output_tokens: Option<u64>,
    pub(crate) cache: Option<Cache<'a>>,
}

#[derive(Clone, Copy)]
pub(crate) struct Cache<'a> {
    pub(crate) key: &'a str,
    pub(crate) window: u64,
}

impl Request<'_> {
    fn same_model(&self, assistant: &Assistant) -> bool {
        assistant.protocol == self.provider.protocol()
            && assistant.provider == self.provider.name()
            && assistant.model == self.model
    }
}

type Trace = crate::record::diagnostics::Trace;

#[derive(Clone, Copy)]
pub(crate) enum Delta<'a> {
    MessageBoundary,
    Text(&'a str),
    Reasoning(&'a str),
    ToolBytes(usize),
}

struct Decoder {
    framing: sse::Decoder,
    bytes: usize,
    events: Events,
}

enum Events {
    Responses(responses::Stream),
    Chat(chat::Stream),
    Messages(messages::Stream),
    Gemini(gemini::Stream),
}

impl Decoder {
    fn new(events: Events) -> Self {
        Self {
            framing: sse::Decoder::default(),
            bytes: 0,
            events,
        }
    }

    fn push(
        &mut self,
        chunk: &[u8],
        on_delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Option<Assistant>, Error> {
        let mut data = Vec::new();
        let framing = self.framing.push(chunk, |event| data.push(event));
        for data in data {
            if data != "[DONE]" {
                self.bytes = self.bytes.saturating_add(data.len());
                if self.bytes > RESPONSE_BYTES {
                    return Err(Error::Limit(format!(
                        "response larger than {} MiB",
                        RESPONSE_BYTES / (1024 * 1024)
                    )));
                }
            }
            let done = match &mut self.events {
                Events::Responses(stream) => stream.event(&data, on_delta),
                Events::Chat(stream) => stream.event(&data, on_delta),
                Events::Messages(stream) => stream.event(&data, on_delta),
                Events::Gemini(stream) => stream.event(&data, on_delta),
            }?;
            if done.is_some() {
                return Ok(done);
            }
        }
        framing.map_err(|error| Error::Limit(error.to_string()))?;
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) struct Limits {
    pub(crate) short: Option<Window>,
    pub(crate) long: Option<Window>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Window {
    pub(crate) used_percent: f64,
    pub(crate) minutes: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Observation {
    Limits(Limits),
}

pub(crate) trait Observer: Send {
    fn delta(&mut self, delta: Delta<'_>);
    fn observation(&mut self, _observation: Observation) {}
    fn flush(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

impl<F: FnMut(Delta<'_>) + Send> Observer for F {
    fn delta(&mut self, delta: Delta<'_>) {
        self(delta);
    }

    async fn flush(&mut self) {}
}

fn finish(
    result: Result<Assistant, Error>,
    mut trace: Option<Trace>,
    credential: Option<&Credential>,
) -> Result<Assistant, Error> {
    use crate::record::diagnostics::Outcome;
    if let Some(trace) = &mut trace {
        trace.finish(match &result {
            Ok(_) => Outcome::Completed,
            Err(Error::Transport(_)) => Outcome::TransportError,
            Err(Error::Http { .. }) => Outcome::HttpError,
            Err(Error::Interrupted(_)) => Outcome::Interrupted,
            Err(Error::Invalid(_)) => Outcome::InvalidResponse,
            Err(Error::Limit(_)) => Outcome::LimitExceeded,
            Err(Error::Failed { .. }) => Outcome::ProviderFailed,
            Err(Error::Incomplete(_)) => Outcome::Incomplete,
            Err(Error::ContextOverflow(_)) => Outcome::ContextOverflow,
            Err(Error::WrongCredential) => Outcome::CredentialError,
        });
    }
    result.map_err(|error| error.sanitized(credential))
}

fn validate(request: &Request<'_>, assistant: &Assistant) -> Result<(), Error> {
    let mut ids = std::collections::HashSet::new();
    for part in &assistant.parts {
        if let crate::history::Content::ToolCall(call) = &part.content {
            if matches!(request.tools, Tools::Disabled(_)) {
                return Err(Error::Invalid(
                    "tool call in a request with tools disabled".into(),
                ));
            }
            if call.id.is_empty() || call.name.is_empty() {
                return Err(Error::Invalid("tool call without id or name".into()));
            }
            if !ids.insert(call.id.as_str()) {
                return Err(Error::Invalid(format!(
                    "tool call {} appears twice",
                    call.id
                )));
            }
        }
    }
    Ok(())
}

fn deliver(trace: Option<&mut Trace>, observer: &mut impl Observer, delta: Delta<'_>) {
    if let Some(trace) = trace {
        match delta {
            Delta::Text(text) => trace.generation(text.len(), 0, 0),
            Delta::Reasoning(text) => trace.generation(0, text.len(), 0),
            Delta::ToolBytes(bytes) => trace.generation(0, 0, bytes),
            Delta::MessageBoundary => {}
        }
    }
    observer.delta(delta);
}

async fn observe<F: std::future::Future>(
    future: F,
    trace: Option<&mut crate::record::diagnostics::Trace>,
) -> F::Output {
    let Some(trace) = trace else {
        return future.await;
    };
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            () = time::sleep_until(trace.next_report().into()) => trace.snapshot("progress"),
        }
    }
}

fn limits(headers: &header::HeaderMap) -> Limits {
    let text = |name: String| headers.get(name)?.to_str().ok().map(str::trim);
    let window = |which: &str| {
        Some(Window {
            used_percent: text(format!("x-codex-{which}-used-percent"))?
                .parse()
                .ok()?,
            minutes: text(format!("x-codex-{which}-window-minutes"))
                .and_then(|minutes| minutes.parse().ok()),
        })
    };
    Limits {
        short: window("primary"),
        long: window("secondary"),
    }
}

async fn observe_limits(headers: &header::HeaderMap, observer: &mut impl Observer) {
    let limits = limits(headers);
    if limits.short.is_some() || limits.long.is_some() {
        observer.observation(Observation::Limits(limits));
        observer.flush().await;
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs().cast_signed())
}

pub(crate) async fn credential(
    store: &Store,
    client: &Client,
    provider: &Provider,
) -> Result<Option<Credential>, String> {
    if matches!(provider.auth(), Auth::None | Auth::External) {
        return Ok(None);
    }
    let key = provider.name();
    let missing = || format!("no credentials for {key}; run `{}`", provider.login_hint());
    let loaded = store.load().map_err(|error| error.to_string())?;
    let credential = match (provider.auth(), loaded.providers.get(key)) {
        (Auth::Chatgpt, Some(Credential::Chatgpt(tokens))) if tokens.needs_refresh() => {
            auth::chatgpt::credential(store, client, key, missing())
                .await
                .map_err(|error| error.to_string())
        }
        (Auth::Chatgpt, Some(credential @ Credential::Chatgpt(_)))
        | (Auth::ApiKey, Some(credential @ Credential::ApiKey { .. })) => Ok(credential.clone()),
        _ => Err(missing()),
    };
    credential.map(Some)
}

#[cfg(test)]
pub(crate) mod mock;
