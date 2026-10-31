use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use std::fmt::Write as _;

use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::{Custom, Protocol, Provider};

pub(crate) fn completed(model: &str, output: &[Value]) -> Value {
    serde_json::json!({"type":"response.completed", "sequence_number":10, "response":{
        "id":"resp_fixture", "object":"response", "created_at":1, "model":model,
        "status":"completed", "error":null, "incomplete_details":null, "output":output,
    }})
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) authorization: Option<&'static str>,
    pub(crate) delay: Duration,
    pub(crate) chunks: Vec<Value>,
    pub(crate) ends: bool,
}

impl Reply {
    pub(crate) fn text(text: &str) -> Self {
        Self::now(vec![
            serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": text } }] }),
            serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ])
    }

    pub(crate) fn python(id: &str, arguments: &Value) -> Self {
        Self::now(vec![
            serde_json::json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": 0, "id": id, "type": "function",
                    "function": { "name": "python", "arguments": arguments.to_string() },
                }] } }] }),
            serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        ])
    }

    pub(crate) fn cut_off(text: &str) -> Self {
        let mut reply = Self::now(vec![
            serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": text } }] }),
        ]);
        reply.ends = false;
        reply
    }

    fn now(chunks: Vec<Value>) -> Self {
        Self {
            status: 200,
            authorization: None,
            delay: Duration::ZERO,
            chunks,
            ends: true,
        }
    }
}

impl Reply {
    fn body(self, status: u16) -> String {
        if status != 200 {
            return self
                .chunks
                .first()
                .map_or_else(String::new, Value::to_string);
        }
        let mut body = String::new();
        for chunk in self.chunks {
            let _ = write!(body, "data: {chunk}\n\n");
        }
        if self.ends {
            body.push_str("data: [DONE]\n\n");
        }
        body
    }
}

pub(crate) async fn serve(
    config_dir: &Path,
    replies: Vec<Reply>,
) -> (Provider, Arc<Mutex<Vec<Value>>>) {
    let (provider, requests, _, _) =
        serve_catalog(
            config_dir,
            replies,
            serde_json::json!({"data": [{"id": "m", "context_window": 1_000_000, "max_tokens": 32_000}]}),
        )
        .await;
    (provider, requests)
}

pub(crate) async fn serve_chatgpt_catalog(
    config_dir: &Path,
    replies: Vec<Reply>,
    catalog: Value,
) -> (
    Provider,
    Arc<Mutex<Vec<Value>>>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Arc<Mutex<Vec<String>>>,
) {
    let (provider, requests, raw, headers) = serve_catalog(config_dir, replies, catalog).await;
    let provider = Provider {
        endpoint: provider.endpoint,
        kind: super::Kind::Chatgpt,
    };
    (provider, requests, raw, headers)
}

pub(crate) async fn serve_catalog(
    config_dir: &Path,
    replies: Vec<Reply>,
    catalog: Value,
) -> (
    Provider,
    Arc<Mutex<Vec<Value>>>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Arc<Mutex<Vec<String>>>,
) {
    let headers = Arc::new(Mutex::new(Vec::new()));
    let heads_seen = Arc::clone(&headers);
    let raw = Arc::new(Mutex::new(Vec::new()));
    let raw_seen = Arc::clone(&raw);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    tokio::spawn(async move {
        let mut replies = std::collections::VecDeque::from(replies);
        while !replies.is_empty() {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = vec![0; 65536];
            let body_start = loop {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let head = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
            heads_seen.lock().unwrap().push(head.clone());
            if head.contains("upgrade: websocket") {
                let _ = socket
                    .write_all(b"HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
                continue;
            }
            if head.starts_with("get ") {
                let body = catalog.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                continue;
            }
            let reply = replies.pop_front().unwrap();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().unwrap());
            while request.len() < body_start + length {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            raw_seen
                .lock()
                .unwrap()
                .push(request[body_start..].to_vec());
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(&request[body_start..]).unwrap());
            tokio::time::sleep(reply.delay).await;
            let status = if reply.authorization.is_some_and(|value| {
                !head
                    .lines()
                    .any(|line| line == format!("authorization: {}", value.to_lowercase()))
            }) {
                401
            } else {
                reply.status
            };
            let body = reply.body(status);
            let content_type = if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    let custom = Custom {
        protocol: Protocol::Chat,
        url,
        key: false,
    };
    super::CustomUpdate::prepare(config_dir, "mock", Some(custom))
        .await
        .unwrap()
        .save()
        .unwrap();
    (
        Provider::named("mock", config_dir).unwrap(),
        requests,
        raw,
        headers,
    )
}
