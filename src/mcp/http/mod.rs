// rmcp's HTTP client reads JSON and error bodies without a bound
// (modelcontextprotocol/rust-sdk#1246), so responses are bounded here.
use std::{collections::HashMap, sync::Arc};

use futures_util::{StreamExt as _, stream};
use reqwest::{
    Client, Response, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE},
};
use rmcp::{
    model::{ClientJsonRpcMessage, ClientRequest, ErrorData, ServerJsonRpcMessage},
    transport::{
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, StreamableHttpClient, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};

const JSON_LIMIT: usize = super::MAX_LINE_BYTES;
const ERROR_LIMIT: usize = 64 * 1024;
const ACCEPT_TYPES: &str = "text/event-stream, application/json";
type Error = StreamableHttpError<reqwest::Error>;

#[derive(Clone)]
pub(super) struct Http(pub(super) Client);

fn unexpected(message: impl Into<String>) -> Error {
    Error::UnexpectedServerResponse(message.into().into())
}

async fn body(response: Response, limit: usize) -> Result<Vec<u8>, Error> {
    use crate::http_body::{self, Mode};
    http_body::read(
        response,
        limit,
        Mode::Complete,
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
    )
    .await
    .complete()
    .map_err(|error| match error {
        http_body::Error::Transport(error) => Error::Client(error),
        http_body::Error::Overflow => {
            unexpected(format!("HTTP body exceeded maximum size of {limit} bytes"))
        }
        http_body::Error::Deadline => unexpected("HTTP response body timed out"),
    })
}

fn scope(header: &str) -> Option<String> {
    let offset = header.to_ascii_lowercase().find("scope=")? + 6;
    let value = &header[offset..];
    if let Some(quoted) = value.strip_prefix('"') {
        Some(quoted[..quoted.find('"')?].to_owned())
    } else {
        let end = value
            .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .unwrap_or(value.len());
        let value = &value[..end];
        if value.is_empty() {
            return None;
        }
        Some(value.to_owned())
    }
}

fn challenge(response: &Response) -> Result<(), Error> {
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        && let Some(value) = response.headers().get(WWW_AUTHENTICATE)
    {
        let value = value
            .to_str()
            .map_err(|_| unexpected("invalid www-authenticate header value"))?;
        return Err(if status == StatusCode::UNAUTHORIZED {
            Error::AuthRequired(AuthRequiredError::new(value.to_owned()))
        } else {
            Error::InsufficientScope(InsufficientScopeError::new(value.to_owned(), scope(value)))
        });
    }
    Ok(())
}

fn sse(response: Response, limit: usize) -> BoxedSseResponse {
    let bytes = stream::unfold(
        (response, Some(EventSize::default())),
        move |(mut response, size)| async move {
            let mut size = size?;
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if size.accept(&chunk, limit) {
                        Some((Ok(chunk), (response, Some(size))))
                    } else {
                        Some((
                            Err(std::io::Error::other(format!(
                                "SSE event exceeded maximum size of {limit} bytes"
                            ))),
                            (response, None),
                        ))
                    }
                }
                Ok(None) => None,
                Err(error) => Some((Err(std::io::Error::other(error)), (response, None))),
            }
        },
    );
    sse_stream::SseStream::from_bytes_stream(bytes).boxed()
}

#[derive(Default)]
struct EventSize {
    retained: usize,
    line: usize,
    comment: bool,
    cr: bool,
}

impl EventSize {
    fn accept(&mut self, bytes: &[u8], limit: usize) -> bool {
        for &byte in bytes {
            if std::mem::take(&mut self.cr) && byte == b'\n' {
                continue;
            }
            if matches!(byte, b'\r' | b'\n') {
                if self.line == 0 {
                    self.retained = 0;
                } else if !self.comment {
                    self.retained = self.retained.saturating_add(self.line).saturating_add(1);
                }
                self.line = 0;
                self.cr = byte == b'\r';
            } else {
                if self.line == 0 {
                    self.comment = byte == b':';
                }
                self.line = self.line.saturating_add(1);
            }
            if self.retained.saturating_add(self.line) > limit {
                return false;
            }
        }
        true
    }
}

impl StreamableHttpClient for Http {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, Error> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_token,
            custom_headers,
            JSON_LIMIT,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, Error> {
        self.0
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_token,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        self.0
            .delete_session(uri, session, auth_token, custom_headers)
            .await
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_token,
            custom_headers,
            JSON_LIMIT,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let mut request = self.0.post(uri.as_ref()).header(ACCEPT, ACCEPT_TYPES);
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        for (name, value) in custom_headers {
            if matches!(name.as_str(), "accept" | "mcp-session-id" | "last-event-id") {
                return Err(Error::ReservedHeaderConflict(name.to_string()));
            }
            request = request.header(name, value);
        }
        if let Some(session) = &session_id {
            request = request.header("mcp-session-id", session.as_ref());
        }
        let response = request.json(&message).send().await?;
        challenge(&response)?;
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_id.is_some() {
            return Err(Error::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let returned_session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let is_request = matches!(message, ClientJsonRpcMessage::Request(_));
        if status.is_success() && response.content_length() == Some(0) && !is_request {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            let bytes = body(response, ERROR_LIMIT).await?;
            let text = String::from_utf8_lossy(&bytes);
            let parsed = serde_json::from_slice::<ServerJsonRpcMessage>(&bytes)
                .ok()
                .filter(|value| matches!(value, ServerJsonRpcMessage::Error(_)));
            if session_id.is_none()
                && status.is_client_error()
                && !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                && let ClientJsonRpcMessage::Request(request) = &message
                && matches!(request.request, ClientRequest::DiscoverRequest(_))
            {
                let error = if let Some(ServerJsonRpcMessage::Error(error)) = parsed {
                    error.error
                } else {
                    ErrorData::invalid_request(
                        format!("server/discover rejected with HTTP {status}: {text}"),
                        None,
                    )
                };
                return Ok(StreamableHttpPostResponse::Json(
                    ServerJsonRpcMessage::error(error, Some(request.id.clone())),
                    None,
                ));
            }
            if content_type
                .as_deref()
                .is_some_and(|value| value.starts_with("application/json"))
                && let Some(parsed) = parsed
            {
                return Ok(StreamableHttpPostResponse::Json(parsed, returned_session));
            }
            return Err(unexpected(format!("HTTP {status}: {text}")));
        }
        match content_type.as_deref() {
            Some(value) if value.starts_with("text/event-stream") => {
                Ok(StreamableHttpPostResponse::Sse(
                    sse(response, max_sse_event_size),
                    returned_session,
                ))
            }
            Some(value) if value.starts_with("application/json") => {
                let bytes = body(response, JSON_LIMIT).await?;
                match serde_json::from_slice(&bytes) {
                    Ok(parsed) => Ok(StreamableHttpPostResponse::Json(parsed, returned_session)),
                    Err(_) if !is_request => Ok(StreamableHttpPostResponse::Accepted),
                    Err(error) => Err(unexpected(format!(
                        "could not parse JSON response as ServerJsonRpcMessage: {error}: {}",
                        String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
                    ))),
                }
            }
            _ => Err(Error::UnexpectedContentType(content_type)),
        }
    }
}

#[cfg(test)]
mod tests;
