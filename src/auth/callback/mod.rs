use std::time::Duration;

use reqwest::Url;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};

use super::Error;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

pub(crate) struct Received {
    pub(crate) url: Url,
    pub(super) code: String,
}

pub(crate) async fn receive(
    listeners: &[TcpListener],
    path: &str,
    state: &str,
) -> Result<Received, Error> {
    time::timeout(LOGIN_TIMEOUT, wait(listeners, path, state))
        .await
        .map_err(|_| Error::Login("no browser callback within five minutes".into()))?
}

async fn wait(listeners: &[TcpListener], path: &str, state: &str) -> Result<Received, Error> {
    loop {
        let accepts = listeners.iter().map(|listener| Box::pin(listener.accept()));
        let (accepted, _, _) = futures_util::future::select_all(accepts).await;
        let (mut socket, _) = accepted.map_err(Error::Io)?;
        if let Some(result) = exchange(&mut socket, path, state).await {
            return result;
        }
    }
}

async fn exchange(
    socket: &mut TcpStream,
    path: &str,
    state: &str,
) -> Option<Result<Received, Error>> {
    let deadline = time::Instant::now() + EXCHANGE_TIMEOUT;
    let target = time::timeout_at(deadline, request_target(socket))
        .await
        .ok()??;
    let (status, message, result) = match callback(&target, path, state) {
        Callback::Unrelated => ("404 Not Found", "Not found.", None),
        Callback::Code(received) => (
            "200 OK",
            "Authorization received. Return to Bart to finish sign-in.",
            Some(Ok(received)),
        ),
        Callback::Failed(problem) => (
            "400 Bad Request",
            "Sign-in failed; see the terminal.",
            Some(Err(Error::Login(problem))),
        ),
    };
    let _ = time::timeout_at(deadline, respond(socket, status, message)).await;
    result
}

enum Callback {
    Unrelated,
    Code(Received),
    Failed(String),
}

fn callback(target: &str, path: &str, state: &str) -> Callback {
    let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
        return Callback::Unrelated;
    };
    if url.path() != path {
        return Callback::Unrelated;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    // RFC 6749 §10.12: a redirect without this login's state is not an answer to it.
    if param("state").as_deref() != Some(state) {
        return Callback::Unrelated;
    }
    if let Some(error) = param("error") {
        return Callback::Failed(param("error_description").unwrap_or(error));
    }
    match param("code") {
        Some(code) if !code.is_empty() => Callback::Code(Received { url, code }),
        _ => Callback::Failed("the callback carried no authorization code".into()),
    }
}

async fn request_target(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buffer = [0; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 || head.len() + read > MAX_REQUEST_HEAD_BYTES {
            return None;
        }
        head.extend_from_slice(&buffer[..read]);
    }
    let line = head.split(|&b| b == b'\r').next()?;
    let mut parts = std::str::from_utf8(line).ok()?.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_owned()),
        _ => None,
    }
}

async fn respond(socket: &mut TcpStream, status: &str, message: &str) {
    let body = format!("<!doctype html><title>Bart</title><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
}

#[cfg(test)]
mod tests;
