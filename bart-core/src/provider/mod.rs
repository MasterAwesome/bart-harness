use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use tokio::time;

use crate::{
    auth::{self, Credential, Store},
    history::{Assistant, Message, ToolDefinition as Definition},
    preferences::ServiceTier,
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

pub use catalog::{ImageInput, Model, Visibility, model, models};
use catalog::{Page, Paging, data_models};
pub use claude_code::available as claude_code_available;
pub(crate) use config::is_builtin;
pub use config::{CustomUpdate, all, commit_identity, logout};
pub use error::{Error, Retry};
use error::{http_error, quota_retry};
use http::authorize;
pub(crate) use transport::{Transport, stream};

const RESPONSE_BYTES: usize = 128 * 1024 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Chatgpt(Box<Url>),
    Claude,
    Custom {
        name: String,
        protocol: HttpProtocol,
        endpoint: Box<Url>,
        key: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpProtocol {
    Responses,
    Chat,
    Messages,
    Gemini,
}

pub use crate::history::Protocol;

impl From<HttpProtocol> for Protocol {
    fn from(protocol: HttpProtocol) -> Self {
        match protocol {
            HttpProtocol::Responses => Self::Responses,
            HttpProtocol::Chat => Self::Chat,
            HttpProtocol::Messages => Self::Messages,
            HttpProtocol::Gemini => Self::Gemini,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HttpAuth {
    Chatgpt,
    ApiKey,
    None,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HttpEndpoint<'a> {
    name: &'a str,
    url: &'a Url,
    protocol: HttpProtocol,
    auth: HttpAuth,
    codex: bool,
}

impl<'a> HttpEndpoint<'a> {
    pub(super) fn name(self) -> &'a str {
        self.name
    }

    pub(super) fn protocol(self) -> HttpProtocol {
        self.protocol
    }

    pub(super) fn auth(self) -> HttpAuth {
        self.auth
    }

    pub(super) fn codex(self) -> bool {
        self.codex
    }

    pub(super) fn base_url(self) -> &'a Url {
        self.url
    }

    pub(super) fn url(self, path: &str) -> Url {
        let mut url = self.url.clone();
        url.set_path(&format!("{}{path}", url.path().trim_end_matches('/')));
        url
    }

    pub(super) fn stream_url(self, model: &str) -> Result<Url, Error> {
        let mut url = match self.protocol {
            HttpProtocol::Responses => self.url("/responses"),
            HttpProtocol::Chat => self.url("/chat/completions"),
            HttpProtocol::Messages => self.url("/messages"),
            HttpProtocol::Gemini => self.url(&gemini::path(model)?),
        };
        if self.protocol == HttpProtocol::Gemini {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        Ok(url)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    Chatgpt,
    External,
    ApiKey,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Custom {
    pub protocol: HttpProtocol,
    pub url: String,
    pub key: bool,
}

fn at(url: &Url, host: &str, base: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port_or_known_default() == Some(443)
        && url.path().trim_end_matches('/') == base
}

impl Provider {
    pub fn name(&self) -> &str {
        match &self.kind {
            Kind::Chatgpt(_) => "chatgpt",
            Kind::Claude => "claude",
            Kind::Custom { name, .. } => name,
        }
    }

    pub fn label(&self) -> &str {
        if matches!(self.kind, Kind::Claude) {
            "claude-code"
        } else {
            self.name()
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self.kind {
            Kind::Chatgpt(_) => Protocol::Responses,
            Kind::Claude => Protocol::Claude,
            Kind::Custom { protocol, .. } => protocol.into(),
        }
    }

    pub fn auth(&self) -> Auth {
        match self.kind {
            Kind::Chatgpt(_) => Auth::Chatgpt,
            Kind::Claude => Auth::External,
            Kind::Custom { key: true, .. } => Auth::ApiKey,
            Kind::Custom { key: false, .. } => Auth::None,
        }
    }

    pub fn endpoint(&self) -> Option<&Url> {
        match &self.kind {
            Kind::Chatgpt(url) | Kind::Custom { endpoint: url, .. } => Some(url),
            Kind::Claude => None,
        }
    }

    pub(super) fn http(&self) -> Option<HttpEndpoint<'_>> {
        match &self.kind {
            Kind::Chatgpt(url) => Some(HttpEndpoint {
                name: "chatgpt",
                url,
                protocol: HttpProtocol::Responses,
                auth: HttpAuth::Chatgpt,
                codex: true,
            }),
            Kind::Custom {
                name,
                protocol,
                endpoint,
                key,
            } => Some(HttpEndpoint {
                name,
                url: endpoint,
                protocol: *protocol,
                auth: if *key {
                    HttpAuth::ApiKey
                } else {
                    HttpAuth::None
                },
                codex: false,
            }),
            Kind::Claude => None,
        }
    }

    fn supports_tier(&self, model: Option<&Model>, tier: ServiceTier) -> bool {
        self.protocol() == Protocol::Responses
            && (tier == ServiceTier::Auto
                || model
                    .and_then(|m| m.service_tiers.as_ref())
                    .is_none_or(|tiers| tiers.contains(&tier)))
    }

    pub fn service_tiers(&self, model: Option<&Model>) -> Vec<ServiceTier> {
        ServiceTier::ALL
            .into_iter()
            .filter(|&tier| self.supports_tier(model, tier))
            .collect()
    }

    pub(crate) fn effective_tier(&self, model: Option<&Model>, tier: ServiceTier) -> ServiceTier {
        if self.supports_tier(model, tier) {
            tier
        } else {
            ServiceTier::Auto
        }
    }

    pub fn capabilities(&self) -> Capabilities {
        let Some(http) = self.http() else {
            return Capabilities {
                output_limit: true,
                web_search: false,
            };
        };
        let web_search = http.codex()
            || match http.protocol() {
                HttpProtocol::Responses => responses::openai_api(http.url),
                HttpProtocol::Chat => at(http.url, "openrouter.ai", "/api/v1"),
                HttpProtocol::Messages => at(http.url, "api.anthropic.com", "/v1"),
                HttpProtocol::Gemini => {
                    at(http.url, "generativelanguage.googleapis.com", "/v1beta")
                }
            };
        Capabilities {
            output_limit: !http.codex(),
            web_search,
        }
    }

    fn builtin() -> Self {
        Self {
            kind: Kind::Chatgpt(Box::new(
                Url::parse("https://chatgpt.com/backend-api/codex")
                    .expect("built-in HTTP endpoint"),
            )),
        }
    }

    fn claude() -> Self {
        Self { kind: Kind::Claude }
    }

    pub fn custom(name: &str, custom: &Custom) -> Result<Self, String> {
        if name.is_empty()
            || name.starts_with('-')
            || name.contains(':')
            || name.chars().any(crate::terminal_text::unsafe_character)
        {
            return Err("a custom provider needs a nonempty, printable single-line name without ':' or a leading '-'".into());
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
            kind: Kind::Custom {
                name: name.into(),
                protocol: custom.protocol,
                endpoint: Box::new(url),
                key: custom.key,
            },
        })
    }

    #[must_use]
    pub fn login_hint(&self) -> String {
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
pub struct Capabilities {
    pub output_limit: bool,
    pub web_search: bool,
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
    pub(crate) service_tier: ServiceTier,
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

type Trace = crate::diagnostics::Trace;

fn retry_kind(retry: Retry) -> crate::diagnostics::RetryKind {
    match retry {
        Retry::Reset(_) => crate::diagnostics::RetryKind::Reset,
        Retry::After(_) => crate::diagnostics::RetryKind::After,
        Retry::No => crate::diagnostics::RetryKind::No,
    }
}

#[derive(Clone, Copy)]
pub enum Delta<'a> {
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
pub struct Limits {
    pub short: Option<Window>,
    pub long: Option<Window>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    pub minutes: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub enum Observation {
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
    use crate::diagnostics::Outcome;
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
            Err(Error::Credential(_)) => Outcome::CredentialError,
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

async fn observe<F: std::future::Future>(future: F, trace: Option<&mut Trace>) -> F::Output {
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

async fn credential(
    client: &Client,
    store: &Store,
    provider: &Provider,
) -> Result<Option<Credential>, Error> {
    if matches!(provider.auth(), Auth::None | Auth::External) {
        return Ok(None);
    }
    let key = provider.name();
    let missing = || format!("no credentials for {key}; run `{}`", provider.login_hint());
    let loaded = store
        .load()
        .map_err(|error| Error::Credential(error.to_string()))?;
    let credential = match (provider.auth(), loaded.providers.get(key)) {
        (Auth::Chatgpt, Some(Credential::Chatgpt(tokens))) if tokens.needs_refresh() => {
            auth::chatgpt::credential(store, client, key, missing())
                .await
                .map_err(|error| Error::Credential(error.to_string()))
        }
        (Auth::Chatgpt, Some(credential @ Credential::Chatgpt(_)))
        | (Auth::ApiKey, Some(credential @ Credential::ApiKey { .. })) => Ok(credential.clone()),
        _ => Err(Error::Credential(missing())),
    };
    credential.map(Some)
}

#[cfg(any(test, feature = "test-support"))]
pub mod mock;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_configs_and_protocols_keep_http_messages_distinct_from_claude_subprocess() {
        for (wire, expected) in [
            ("responses", HttpProtocol::Responses),
            ("chat", HttpProtocol::Chat),
            ("messages", HttpProtocol::Messages),
            ("gemini", HttpProtocol::Gemini),
        ] {
            let custom: Custom = serde_json::from_str(&format!(
                r#"{{"protocol":"{wire}","url":"https://example.invalid/v1","key":true}}"#
            ))
            .unwrap();
            assert_eq!(custom.protocol, expected);
            let provider = Provider::custom("custom", &custom).unwrap();
            assert_eq!(provider.protocol(), Protocol::from(expected));
            assert!(provider.http().is_some());
        }

        assert!(
            serde_json::from_str::<Custom>(
                r#"{"protocol":"claude","url":"https://example.invalid/v1","key":false}"#
            )
            .is_err(),
            "claude subprocess must not deserialize as a custom HTTP provider"
        );

        let claude = Provider::claude();
        assert_eq!(claude.protocol(), Protocol::Claude);
        assert!(claude.http().is_none());

        let anthropic = Provider::custom(
            "anthropic",
            &Custom {
                protocol: HttpProtocol::Messages,
                url: "https://api.anthropic.com/v1".into(),
                key: true,
            },
        )
        .unwrap();
        assert_eq!(anthropic.protocol(), Protocol::Messages);
        assert_eq!(anthropic.http().unwrap().protocol(), HttpProtocol::Messages);
        assert!(anthropic.capabilities().web_search);
        assert!(!claude.capabilities().web_search);

        for (wire, expected) in [
            ("responses", Protocol::Responses),
            ("chat", Protocol::Chat),
            ("messages", Protocol::Messages),
            ("gemini", Protocol::Gemini),
            ("claude", Protocol::Claude),
        ] {
            let parsed: Protocol = serde_json::from_str(&format!(r#""{wire}""#)).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!(r#""{wire}""#)
            );
        }
    }

    #[tokio::test]
    async fn invalid_custom_provider_can_be_removed_without_blocking_valid_entries() {
        let dir = crate::tool::test_support::TempDir::new("provider-config-recovery");
        std::fs::write(
            dir.path().join("providers.json"),
            r#"{
                "valid": {"protocol": "chat", "url": "https://example.invalid/v1", "key": false},
                "bad": {"protocol": "claude", "url": "https://example.invalid/v1", "key": false},
                "ambiguous": {"protocol": "chat", "url": "https://example.invalid/v1", "key": true, "key": false}
            }"#,
        )
        .unwrap();

        let valid = Provider::named("valid", dir.path()).unwrap();
        assert_eq!(valid.protocol(), Protocol::Chat);
        assert!(Provider::named("bad", dir.path()).is_err());
        let duplicate = Provider::named("ambiguous", dir.path()).unwrap_err();
        assert!(duplicate.contains("duplicate field"), "{duplicate}");

        let store = Store::new(dir.path().join("auth.json"));
        logout(dir.path(), &store, "bad").await.unwrap();
        logout(dir.path(), &store, "ambiguous").await.unwrap();

        assert!(Provider::named("bad", dir.path()).is_err());
        assert!(Provider::named("ambiguous", dir.path()).is_err());
        let remaining = Provider::named("valid", dir.path()).unwrap();
        assert_eq!(remaining, valid);
        assert!(all(dir.path()).unwrap().contains(&valid));
    }
}
