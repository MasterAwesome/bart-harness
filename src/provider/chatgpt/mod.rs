use std::borrow::Cow;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::Mutex, time};

use reqwest::Client;

use super::{
    Cache, Delta, Error, IDLE_TIMEOUT, Limits, Observer, RESPONSE_BYTES, Request, StatusCode,
    Trace, Window, deliver, error_message, exhausted_reset, http_retry, observe, observe_limits,
    prepare::{self, Snapshot},
    responses, unix_now, validate,
};
use crate::{
    auth::{Credential, chatgpt},
    history::{Assistant, Message as Item},
};

const BETA: &str = "responses_websockets=2026-02-06";
const TURN_STATE: &str = "x-codex-turn-state";
const CONTINUATION_LOST: [&str; 2] = [
    "previous_response_not_found",
    "websocket_connection_limit_reached",
];

#[derive(Default)]
pub(super) struct Route(Mutex<State>);

#[derive(Default)]
struct State {
    owner: Option<String>,
    connection: Option<Connection>,
    turn: String,
    turn_state: Option<String>,
    http: bool,
}

struct Connection {
    sender: soketto::connection::Sender<upgrade::Stream>,
    receiver: soketto::connection::Receiver<upgrade::Stream>,
    cache: Option<(String, u64)>,
    last: Option<Last>,
}

// What the server stored for `response_id`: the history we sent, its output and the options used.
struct Last {
    response_id: String,
    options: Value,
    prefix: Vec<Item>,
    images: bool,
}

struct Prepared {
    text: String,
    options: Value,
    history: Vec<Item>,
    images: bool,
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
}

enum Outcome {
    Done(Assistant),
    Http,
}

enum Read {
    Done(Assistant, String),
    Lost(String),
}

impl super::Route for Route {
    async fn send(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        on_delta: &mut impl Observer,
        mut trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error> {
        match receive(
            self,
            credential,
            request,
            true,
            on_delta,
            trace.as_deref_mut(),
        )
        .await?
        {
            Outcome::Done(assistant) => Ok(assistant),
            Outcome::Http => {
                super::Http
                    .send(client, credential, request, on_delta, trace)
                    .await
            }
        }
    }

    async fn prewarm(
        &self,
        credential: Option<&Credential>,
        request: &Request<'_>,
    ) -> Result<(), Error> {
        let mut ignore = |_: Delta<'_>| {};
        receive(self, credential, request, false, &mut ignore, None)
            .await
            .map(drop)
    }

    async fn begin_turn(&self) -> Result<(), Error> {
        let id = crate::random::bytes::<16>()
            .map_err(|error| Error::Invalid(format!("no randomness: {error}")))?;
        let mut state = self.0.lock().await;
        state.turn_state = None;
        state.turn = id.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(())
    }

    async fn fall_back(&self) -> bool {
        let mut state = self.0.lock().await;
        state.connection = None;
        !std::mem::replace(&mut state.http, true)
    }
}

async fn receive(
    route: &Route,
    credential: Option<&Credential>,
    request: &Request<'_>,
    generate: bool,
    on_delta: &mut impl Observer,
    mut trace: Option<&mut Trace>,
) -> Result<Outcome, Error> {
    let Some(Credential::Chatgpt(tokens)) = credential else {
        return Err(Error::WrongCredential);
    };
    let mut state = route.0.lock().await;
    let state = &mut *state;
    if state.owner.as_deref() != Some(tokens.account_id.as_str()) {
        state.connection = None;
        state.turn_state = None;
        state.owner = Some(tokens.account_id.clone());
    }
    if state.http {
        return Ok(Outcome::Http);
    }
    let mut connection = state
        .connection
        .take()
        .filter(|connection| connection.cache == owned(request.cache));
    let mut retried = false;
    loop {
        let mut connection = match connection.take() {
            Some(connection) => connection,
            None => match connect(request, tokens, &mut state.turn_state, on_delta).await? {
                Some(connection) => connection,
                None => {
                    state.http = true;
                    return Ok(Outcome::Http);
                }
            },
        };
        let mut metadata = request
            .cache
            .map_or(Value::Null, |cache| metadata(cache, tokens));
        metadata["turn_id"] = state.turn.as_str().into();
        if let Some(turn_state) = &state.turn_state {
            metadata[TURN_STATE] = turn_state.as_str().into();
        }
        let last = connection.last.take();
        let slot = prepare::slot().await;
        let snapshot = Snapshot::new(request);
        let prepared = slot
            .run(move || frame(snapshot, last, metadata, generate))
            .await?;
        if let Some(trace) = &mut trace {
            trace.body(prepared.text.len());
        }
        let sent = time::timeout(IDLE_TIMEOUT, async {
            connection.sender.send_text_owned(prepared.text).await?;
            connection.sender.flush().await
        })
        .await
        .map_err(|_| Error::Interrupted("websocket send timed out".into()))?;
        let read = match sent {
            Ok(()) => {
                read(
                    &mut connection,
                    &mut state.turn_state,
                    request,
                    on_delta,
                    trace.as_deref_mut(),
                )
                .await?
            }
            Err(error) => Read::Lost(format!("send failed: {error}")),
        };
        match read {
            Read::Done(assistant, response_id) => {
                let mut prefix = prepared.history;
                if generate {
                    prefix.push(Item::Assistant(assistant.clone()));
                }
                connection.last = (!response_id.is_empty()).then_some(Last {
                    response_id,
                    options: prepared.options,
                    prefix,
                    images: prepared.images,
                });
                state.connection = Some(connection);
                return Ok(Outcome::Done(assistant));
            }
            Read::Lost(_) if !retried => retried = true,
            Read::Lost(reason) => {
                return Err(Error::Interrupted(format!(
                    "the websocket was lost twice before a response: {reason}"
                )));
            }
        }
    }
}

// Continue from `last` only when the history extends it exactly, the shared part has no
// images and the options are unchanged. An image path doesn't prove the file is unchanged.
fn frame(
    snapshot: Snapshot,
    last: Option<Last>,
    metadata: Value,
    generate: bool,
) -> Result<Prepared, Error> {
    let image = |error: std::io::Error| Error::Failed {
        code: "image_file".into(),
        message: error.to_string(),
    };
    let options = responses::body(&snapshot.request(&[])).map_err(image)?;
    let previous = last.filter(|last| {
        !last.images && last.options == options && snapshot.history.starts_with(&last.prefix)
    });
    let suffix = previous.as_ref().map_or(&snapshot.history[..], |last| {
        &snapshot.history[last.prefix.len()..]
    });
    let mut frame = options.clone();
    frame["input"] = Value::Array(responses::input(&snapshot.request(suffix)).map_err(image)?);
    frame["type"] = "response.create".into();
    if !generate {
        frame["generate"] = false.into();
    }
    if let Some(last) = &previous {
        frame["previous_response_id"] = last.response_id.as_str().into();
    }
    frame["client_metadata"] = metadata;
    let text = serde_json::to_string(&frame)
        .map_err(|error| Error::Invalid(format!("cannot encode the request: {error}")))?;
    let images = snapshot
        .history
        .iter()
        .any(|message| matches!(message, Item::Image(_)));
    Ok(Prepared {
        text,
        options,
        history: snapshot.history,
        images,
    })
}

fn owned(cache: Option<Cache<'_>>) -> Option<(String, u64)> {
    cache.map(|cache| (cache.key.to_owned(), cache.window))
}

pub(super) fn identity(request: &Request<'_>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("x-codex-routing-hint", format!("model={}", request.model))];
    if let Some(cache) = request.cache {
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            headers.push((name, cache.key.to_owned()));
        }
        headers.push(("x-codex-window-id", window(cache)));
    }
    headers
}

pub(super) fn metadata(cache: Cache<'_>, tokens: &chatgpt::Tokens) -> Value {
    json!({
        "x-codex-installation-id": tokens.installation_id,
        "session_id": cache.key,
        "thread_id": cache.key,
        "x-codex-window-id": window(cache),
    })
}

fn window(cache: Cache<'_>) -> String {
    format!("{}:{}", cache.key, cache.window)
}

async fn connect(
    request: &Request<'_>,
    tokens: &chatgpt::Tokens,
    turn_state: &mut Option<String>,
    on_delta: &mut impl Observer,
) -> Result<Option<Connection>, Error> {
    let url = request.provider.stream_url(request.model)?;
    let mut headers = HeaderMap::new();
    let mut add = |name: &'static str, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    };
    add(
        "authorization",
        &format!("Bearer {}", tokens.access.expose()),
    );
    add("chatgpt-account-id", &tokens.account_id);
    add("originator", chatgpt::ORIGINATOR);
    add("user-agent", chatgpt::USER_AGENT);
    add("openai-beta", BETA);
    if let Some(residency) = &tokens.residency {
        add("x-openai-internal-codex-residency", residency);
    }
    for (name, value) in identity(request) {
        add(name, &value);
    }
    let upgrade = time::timeout(IDLE_TIMEOUT, upgrade::connect(&url, &headers))
        .await
        .map_err(|_| Error::Interrupted("websocket connection timed out".into()))?
        .map_err(Error::Interrupted)?;
    let (sender, receiver, headers) = match upgrade {
        upgrade::Upgrade::Open {
            sender,
            receiver,
            headers,
        } => (sender, receiver, headers),
        upgrade::Upgrade::Refused {
            status,
            headers,
            body,
        } => {
            observe_limits(&headers, on_delta).await;
            if status == StatusCode::UPGRADE_REQUIRED {
                return Ok(None);
            }
            return Err(rejected(status, &headers, &body));
        }
    };
    observe_limits(&headers, on_delta).await;
    if let Some(value) = headers
        .get(TURN_STATE)
        .and_then(|value| value.to_str().ok())
    {
        *turn_state = Some(value.to_owned());
    }
    Ok(Some(Connection {
        sender,
        receiver,
        cache: owned(request.cache),
        last: None,
    }))
}

async fn read(
    connection: &mut Connection,
    turn_state: &mut Option<String>,
    request: &Request<'_>,
    on_delta: &mut impl Observer,
    mut trace: Option<&mut Trace>,
) -> Result<Read, Error> {
    let mut stream = responses::Stream::new(request.provider.name(), request.model, true);
    let mut bytes = 0usize;
    let mut response_id = String::new();
    let mut first = true;
    loop {
        let mut data = Vec::new();
        let received = observe(
            time::timeout(IDLE_TIMEOUT, connection.receiver.receive_data(&mut data)),
            trace.as_deref_mut(),
        )
        .await
        .map_err(|_| Error::Interrupted(format!("no data for {}s", IDLE_TIMEOUT.as_secs())))?;
        let text = match received {
            Ok(soketto::Data::Text(_)) => String::from_utf8(data)
                .map_err(|_| Error::Invalid("websocket text is not UTF-8".into()))?,
            Ok(soketto::Data::Binary(_)) => {
                return Err(Error::Invalid("binary websocket event".into()));
            }
            Err(error) if first => {
                return Ok(Read::Lost(format!("failed before any event: {error}")));
            }
            Err(soketto::connection::Error::Closed) => {
                return Err(Error::Interrupted(
                    "websocket closed before the response completed".into(),
                ));
            }
            Err(error) => return Err(Error::Interrupted(format!("websocket failed: {error}"))),
        };
        if std::mem::take(&mut first)
            && let Some(trace) = &mut trace
        {
            trace.headers(101);
        }
        if let Some(trace) = &mut trace {
            trace.chunk(text.len());
        }
        bytes = bytes.saturating_add(text.len());
        if bytes > RESPONSE_BYTES {
            return Err(Error::Limit(format!(
                "response larger than {} MiB",
                RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        let invalid =
            |error: serde_json::Error| Error::Invalid(format!("invalid websocket event: {error}"));
        let head: Head = serde_json::from_str(&text).map_err(invalid)?;
        let kind = Some(head.kind.as_ref());
        let event: Value = if matches!(
            head.kind.as_ref(),
            "error"
                | "response.failed"
                | "codex.rate_limits"
                | "response.metadata"
                | "response.created"
                | "response.completed"
                | "response.incomplete"
        ) {
            serde_json::from_str(&text).map_err(invalid)?
        } else {
            Value::Null
        };
        if kind == Some("error")
            && event["error"]["code"]
                .as_str()
                .is_some_and(|code| CONTINUATION_LOST.contains(&code))
        {
            return Ok(Read::Lost(format!(
                "server dropped the continuation: {}",
                event["error"]["code"].as_str().unwrap_or_default()
            )));
        }
        let error = match kind {
            Some("error") => &event["error"],
            Some("response.failed") => &event["response"]["error"],
            _ => &Value::Null,
        };
        let usage_limit = ["type", "code"]
            .iter()
            .any(|key| error[*key] == "usage_limit_reached");
        let status = event["status"]
            .as_u64()
            .or_else(|| event["status_code"].as_u64())
            .and_then(|status| u16::try_from(status).ok())
            .and_then(|status| StatusCode::from_u16(status).ok())
            .filter(|status| !status.is_success());
        if matches!(kind, Some("error" | "response.failed")) {
            let mut headers = event_headers(&event["headers"]);
            headers.extend(event_headers(&error["headers"]));
            observe_limits(&headers, on_delta).await;
            if kind == Some("error")
                && let Some(status) = status
            {
                let failure = rejected(status, &headers, text.as_bytes());
                if let Some(trace) = &mut trace {
                    trace.provider_error(
                        false,
                        Some(status.as_u16()),
                        usage_limit,
                        error["resets_at"].as_i64().is_some(),
                        headers.contains_key("retry-after"),
                        failure.retry(),
                    );
                }
                return Err(failure);
            }
        }
        match kind {
            Some("codex.rate_limits") => {
                let window = |which: &str| {
                    let window = &event["rate_limits"][which];
                    Some(Window {
                        used_percent: window["used_percent"].as_f64()?,
                        minutes: window["window_minutes"].as_u64(),
                    })
                };
                on_delta.observation(super::Observation::Limits(Limits {
                    short: window("primary"),
                    long: window("secondary"),
                }));
                on_delta.flush().await;
                continue;
            }
            Some("response.metadata") => {
                if let Some(value) = event["headers"]
                    .as_object()
                    .and_then(|headers| {
                        headers
                            .iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case(TURN_STATE))
                    })
                    .and_then(|(_, value)| value.as_str())
                {
                    *turn_state = Some(value.to_owned());
                }
                continue;
            }
            Some("response.created" | "response.completed" | "response.incomplete") => {
                if let Some(id) = event["response"]["id"].as_str() {
                    id.clone_into(&mut response_id);
                }
            }
            _ => {}
        }
        let result = stream.event(&text, &mut |delta| {
            deliver(trace.as_deref_mut(), on_delta, delta);
        });
        if let Err(failure) = &result
            && matches!(kind, Some("error" | "response.failed"))
            && let Some(trace) = &mut trace
        {
            trace.provider_error(
                kind == Some("response.failed"),
                status.map(|status| status.as_u16()),
                usage_limit,
                error["resets_at"].as_i64().is_some(),
                event_headers(&error["headers"]).contains_key("retry-after"),
                failure.retry(),
            );
        }
        let done = result?;
        on_delta.flush().await;
        if let Some(assistant) = done {
            validate(request, &assistant)?;
            return Ok(Read::Done(assistant, response_id));
        }
    }
}

fn event_headers(value: &Value) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in value.as_object().into_iter().flatten() {
        let text = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&text),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

fn rejected(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Error {
    let now = unix_now();
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok());
    Error::Http {
        status,
        message: error_message(body, now),
        retry: http_retry(status, retry_after, body, exhausted_reset(headers), now),
    }
}

#[cfg(test)]
mod tests;
mod upgrade;
