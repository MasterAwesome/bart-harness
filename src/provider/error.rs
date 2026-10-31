use std::{
    fmt::{self, Write as _},
    time::{Duration, UNIX_EPOCH},
};

use reqwest::{StatusCode, header};
use serde_json::Value;
use tokio::time;

use super::{CONTEXT_LENGTH_EXCEEDED, ERROR_BODY_TIMEOUT, unix_now};
use crate::auth::Credential;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    No,
    After(Option<Duration>),
    Reset(Duration),
}

const LONGEST_WAIT: Duration = Duration::from_secs(120);
const MAX_RESET_WAIT: Duration = Duration::from_hours(31 * 24);

#[derive(Debug)]
pub(crate) enum Error {
    Transport(reqwest::Error),
    Http {
        status: StatusCode,
        message: String,
        retry: Retry,
    },
    Failed {
        code: String,
        message: String,
    },
    Incomplete(String),
    ContextOverflow(String),
    Interrupted(String),
    Invalid(String),
    Limit(String),
    WrongCredential,
}

impl Error {
    // A server that echoes what it was sent would print the credential.
    pub(super) fn sanitized(mut self, credential: Option<&Credential>) -> Self {
        if let Self::Transport(error) = self {
            return Self::Transport(error.without_url());
        }
        let secret = match credential {
            Some(Credential::ApiKey { key }) => key.expose(),
            Some(Credential::Chatgpt(tokens)) => tokens.access.expose(),
            _ => "",
        };
        let clean = |text: &mut String| *text = printable(text, &[secret]);
        match &mut self {
            Self::Http { message, .. }
            | Self::Incomplete(message)
            | Self::ContextOverflow(message)
            | Self::Interrupted(message)
            | Self::Invalid(message)
            | Self::Limit(message) => {
                clean(message);
            }
            Self::Failed { code, message } => {
                clean(code);
                clean(message);
            }
            Self::Transport(_) | Self::WrongCredential => {}
        }
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "request failed: {error}"),
            Self::Http {
                status, message, ..
            } => write!(f, "HTTP {status}: {message}"),
            Self::Failed { code, message } => write!(f, "response failed ({code}): {message}"),
            Self::Incomplete(reason) => write!(f, "response incomplete: {reason}"),
            Self::ContextOverflow(message) => {
                write!(f, "the context window was exceeded: {message}")
            }
            Self::Interrupted(problem) => write!(f, "response interrupted: {problem}"),
            Self::Invalid(problem) => write!(f, "invalid provider response: {problem}"),
            Self::Limit(problem) => write!(f, "response exceeds a safety limit: {problem}"),
            Self::WrongCredential => f.write_str("the credential does not belong to this provider"),
        }
    }
}

impl Error {
    #[must_use]
    pub(crate) fn retry(&self) -> Retry {
        match self {
            Self::Transport(error) if transient(error) => Retry::After(None),
            Self::Interrupted(_) => Retry::After(None),
            Self::Http { retry, .. } => *retry,
            Self::Failed { code, .. }
                if matches!(
                    code.as_str(),
                    "server_error"
                        | "server_is_overloaded"
                        | "overloaded"
                        | "overloaded_error"
                        | "api_error"
                        | "UNAVAILABLE"
                        | "INTERNAL"
                        | "rate_limit_exceeded"
                        | "slow_down"
                ) =>
            {
                Retry::After(None)
            }
            Self::Transport(_)
            | Self::Failed { .. }
            | Self::Incomplete(_)
            | Self::ContextOverflow(_)
            | Self::Invalid(_)
            | Self::Limit(_)
            | Self::WrongCredential => Retry::No,
        }
    }
}

/// reqwest reports a connection lost while streaming the body as a decode error, so only bad
/// JSON or framing (hyper's `InvalidData` and `InvalidInput`) counts as malformed.
fn transient(error: &reqwest::Error) -> bool {
    if error.is_builder() || error.is_redirect() {
        return false;
    }
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if cause.is::<serde_json::Error>()
            || cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput
                )
            })
        {
            return false;
        }
        source = cause.source();
    }
    error.is_timeout()
        || error.is_connect()
        || error.is_request()
        || error.is_body()
        || error.is_decode()
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

pub(super) async fn http_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let reset = exhausted_reset(response.headers());
    let response_retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let collected = crate::http_body::read(
        response,
        MAX_ERROR_BODY_BYTES,
        crate::http_body::Mode::Excerpt,
        time::Instant::now() + ERROR_BODY_TIMEOUT,
    )
    .await;
    let (body, complete) = match collected {
        crate::http_body::Body::Complete(body) => (body, true),
        crate::http_body::Body::Excerpt { bytes, .. } => (bytes, false),
    };
    let now = unix_now();
    if complete
        && status == StatusCode::BAD_REQUEST
        && serde_json::from_slice::<Value>(&body)
            .is_ok_and(|parsed| parsed["error"]["code"] == CONTEXT_LENGTH_EXCEEDED)
    {
        return Error::ContextOverflow(error_message(&body, now));
    }
    Error::Http {
        status,
        message: if complete {
            error_message(&body, now)
        } else {
            format!("{} [incomplete HTTP body]", String::from_utf8_lossy(&body))
        },
        retry: http_retry(
            status,
            response_retry_after.as_deref(),
            if complete { &body } else { &[] },
            reset,
            now,
        ),
    }
}

pub(super) fn exhausted_reset(headers: &header::HeaderMap) -> Option<i64> {
    let value = |name: String| headers.get(name)?.to_str().ok().map(str::trim);
    ["primary", "secondary"]
        .into_iter()
        .filter(|window| {
            value(format!("x-codex-{window}-used-percent"))
                .and_then(|used| used.parse::<f64>().ok())
                .is_some_and(|used| used >= 100.0)
        })
        .filter_map(|window| value(format!("x-codex-{window}-reset-at"))?.parse().ok())
        .max()
}

pub(super) fn http_retry(
    status: StatusCode,
    retry_after: Option<&str>,
    body: &[u8],
    header_reset: Option<i64>,
    now: i64,
) -> Retry {
    let body = serde_json::from_slice::<Value>(body).unwrap_or_default();
    let error = &body["error"];
    let named = |names: &[&str]| {
        [&error["type"], &error["code"]]
            .into_iter()
            .filter_map(Value::as_str)
            .any(|name| names.contains(&name))
    };
    if named(&[
        "insufficient_quota",
        "usage_not_included",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded",
    ]) {
        return Retry::No;
    }
    let usage_limit = named(&["usage_limit_reached"]);
    let resets_at = error["resets_at"]
        .as_i64()
        .or(header_reset.filter(|_| usage_limit));
    let retry_after = retry_after.and_then(|value| retry_after_wait(value.trim(), now));
    quota_retry(status, retry_after, resets_at, usage_limit, now)
}

pub(super) fn quota_retry(
    status: StatusCode,
    retry_after: Option<Duration>,
    resets_at: Option<i64>,
    usage_limit: bool,
    now: i64,
) -> Retry {
    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
        return Retry::No;
    }
    let wait = retry_after.or_else(|| {
        resets_at.map(|resets_at| {
            Duration::from_secs(resets_at.saturating_sub(now).max(0).cast_unsigned())
        })
    });
    let quota = status == StatusCode::TOO_MANY_REQUESTS
        && (usage_limit || resets_at.is_some() || wait.is_some_and(|wait| wait > LONGEST_WAIT));
    match wait {
        Some(wait) if quota && !wait.is_zero() && wait <= MAX_RESET_WAIT => Retry::Reset(wait),
        Some(wait) if wait <= LONGEST_WAIT => Retry::After(Some(wait)),
        None if !usage_limit => Retry::After(None),
        _ => Retry::No,
    }
}

fn retry_after_wait(value: &str, now: i64) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    let at = at.duration_since(UNIX_EPOCH).ok()?.as_secs().cast_signed();
    Some(Duration::from_secs((at - now).max(0).cast_unsigned()))
}

pub(crate) fn redact(text: &str, secrets: &[&str]) -> String {
    let mut secrets: Vec<&str> = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .collect();
    // Longest first, so a secret containing another is not left half shown;
    // one pass, so a marker is never searched again.
    secrets.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    secrets.dedup();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(secret) = secrets.iter().find(|secret| rest.starts_with(**secret)) {
            out.push_str("[redacted]");
            rest = &rest[secret.len()..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

pub(crate) fn printable(text: &str, secrets: &[&str]) -> String {
    crate::terminal_text::safe_text(
        &redact(text, secrets),
        crate::terminal_text::TextMode::Diagnostic,
    )
    .chars()
    .take(500)
    .collect()
}

pub(super) fn error_message(body: &[u8], now: i64) -> String {
    let text = String::from_utf8_lossy(body);
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return text.into_owned();
    };
    let error = &parsed["error"];
    let mut message = error["message"].as_str().unwrap_or(&text).to_owned();
    if let Some(resets_at) = error["resets_at"].as_i64() {
        let wait = resets_at.saturating_sub(now).max(0);
        let _ = write!(
            message,
            " (resets in {}h {}m)",
            wait / 3600,
            wait % 3600 / 60
        );
    }
    message
}
