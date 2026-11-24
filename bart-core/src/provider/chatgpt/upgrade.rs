use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use reqwest::{
    StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::protocol::{Role, WebSocketConfig},
};

// RFC 6455 §1.3.
const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;

pub(super) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(super) type Stream = Box<WebSocketStream<Box<dyn Io>>>;

pub(super) enum Upgrade {
    Open {
        stream: Stream,
        headers: HeaderMap,
    },
    Refused {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
        complete: bool,
    },
}

pub(super) async fn connect(url: &reqwest::Url, headers: &HeaderMap) -> Result<Upgrade, String> {
    let host = url.host_str().ok_or("websocket URL without host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url
        .port_or_known_default()
        .ok_or("websocket URL without port")?;
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|error| format!("cannot connect to {host}: {error}"))?;
    tcp.set_nodelay(true).ok();
    let mut io: Box<dyn Io> = match url.scheme() {
        "https" => {
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|error| format!("invalid host {host}: {error}"))?;
            Box::new(
                tokio_rustls::TlsConnector::from(tls()?)
                    .connect(name, tcp)
                    .await
                    .map_err(|error| format!("TLS with {host} failed: {error}"))?,
            )
        }
        "http" => Box::new(tcp),
        other => return Err(format!("unsupported websocket scheme {other}")),
    };
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| format!("no randomness: {error}"))?;
    let key = base64::engine::general_purpose::STANDARD.encode(nonce);
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let authority = url.authority();
    let mut head = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
    );
    for (name, value) in headers {
        let value = value
            .to_str()
            .map_err(|_| format!("header {name} is not text"))?;
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    io.write_all(head.as_bytes())
        .await
        .map_err(|error| format!("websocket upgrade failed: {error}"))?;
    let (status, headers, rest) = read_head(&mut io).await?;
    if status != StatusCode::SWITCHING_PROTOCOLS {
        let deadline = tokio::time::Instant::now() + super::super::ERROR_BODY_TIMEOUT;
        let (body, complete) = read_body(&mut io, status, &headers, rest, deadline).await;
        return Ok(Upgrade::Refused {
            status,
            headers,
            body,
            complete,
        });
    }
    let tokens = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(|token| token.trim().to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    if !tokens("upgrade").iter().any(|token| token == "websocket")
        || !tokens("connection").iter().any(|token| token == "upgrade")
    {
        return Err("websocket upgrade without Upgrade: websocket and Connection: Upgrade".into());
    }
    if headers.contains_key("sec-websocket-protocol") {
        return Err("websocket upgrade chose a subprotocol that was not offered".into());
    }
    if headers.contains_key("sec-websocket-extensions") {
        return Err("websocket upgrade chose an extension that was not offered".into());
    }
    let expected = base64::engine::general_purpose::STANDARD
        .encode(Sha1::digest(format!("{key}{ACCEPT_GUID}")));
    let mut accepts = headers.get_all("sec-websocket-accept").iter();
    if accepts
        .next()
        .is_none_or(|accept| accept.as_bytes() != expected.as_bytes())
        || accepts.next().is_some()
    {
        return Err("websocket upgrade with a wrong Sec-WebSocket-Accept".into());
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let stream =
        Box::new(WebSocketStream::from_partially_read(io, rest, Role::Client, Some(config)).await);
    Ok(Upgrade::Open { stream, headers })
}

fn tls() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            use rustls_platform_verifier::BuilderVerifierExt as _;
            let config = rustls::ClientConfig::builder()
                .with_platform_verifier()
                .map_err(|error| format!("certificate roots: {error}"))?
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

async fn read_head(io: &mut Box<dyn Io>) -> Result<(StatusCode, HeaderMap, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = io
            .read(&mut chunk)
            .await
            .map_err(|error| format!("websocket upgrade failed: {error}"))?;
        if read == 0 {
            return Err("connection closed during the websocket upgrade".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        let mut fields = vec![httparse::EMPTY_HEADER; 256];
        let mut response = httparse::Response::new(&mut fields);
        match response.parse(&buffer) {
            Ok(httparse::Status::Complete(length)) => {
                let status = response
                    .code
                    .and_then(|code| StatusCode::from_u16(code).ok())
                    .ok_or("websocket upgrade without a status")?;
                let mut headers = HeaderMap::new();
                for field in response.headers.iter() {
                    if let (Ok(name), Ok(value)) = (
                        HeaderName::from_bytes(field.name.as_bytes()),
                        HeaderValue::from_bytes(field.value),
                    ) {
                        headers.append(name, value);
                    }
                }
                return Ok((status, headers, buffer.split_off(length)));
            }
            Ok(httparse::Status::Partial) if buffer.len() < MAX_HEAD_BYTES => {}
            Ok(httparse::Status::Partial) => {
                return Err("websocket upgrade response headers are too large".into());
            }
            Err(error) => return Err(format!("invalid websocket upgrade response: {error}")),
        }
    }
}

async fn read_body(
    io: &mut Box<dyn Io>,
    status: StatusCode,
    headers: &HeaderMap,
    mut body: Vec<u8>,
    deadline: tokio::time::Instant,
) -> (Vec<u8>, bool) {
    if status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        return (Vec::new(), true);
    }
    let chunked = headers
        .get(reqwest::header::TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let content_length = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.trim().parse::<usize>().ok());
    let limit = content_length.map_or(MAX_HEAD_BYTES, |length| length.min(MAX_HEAD_BYTES));
    let mut chunk = [0u8; 4096];
    let mut eof = false;
    loop {
        let complete = if chunked {
            dechunk(&body).is_some()
        } else if let Some(length) = content_length {
            length <= MAX_HEAD_BYTES && body.len() >= length
        } else {
            false
        };
        if complete || body.len() >= MAX_HEAD_BYTES {
            break;
        }
        match tokio::time::timeout_at(deadline, io.read(&mut chunk)).await {
            Ok(Ok(0)) => {
                eof = true;
                break;
            }
            Ok(Err(_)) | Err(_) => break,
            Ok(Ok(read)) => body.extend_from_slice(&chunk[..read]),
        }
    }
    if chunked {
        return match dechunk(&body) {
            Some(decoded) => (decoded, true),
            None => (Vec::new(), false),
        };
    }
    if let Some(length) = content_length {
        let complete = length <= MAX_HEAD_BYTES && body.len() >= length;
        body.truncate(limit);
        return (body, complete);
    }
    let complete = eof && body.len() < MAX_HEAD_BYTES;
    body.truncate(limit);
    (body, complete)
}

fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = raw.windows(2).position(|pair| pair == b"\r\n")?;
        let size = std::str::from_utf8(&raw[..line]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        raw = &raw[line + 2..];
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(raw.get(..size)?);
        raw = raw.get(size + 2..)?;
    }
}
