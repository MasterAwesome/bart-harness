use super::*;
use crate::{auth::Store, tool::test_support::TempDir};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

async fn server(mode: &'static str) -> (Arc<str>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                if socket.read_line(&mut line).await.unwrap() == 0 {
                    return;
                }
                let method = line.split_whitespace().next().unwrap().to_owned();
                let mut length = 0;
                loop {
                    line.clear();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                let mut socket = socket.into_inner();
                if method != "POST" {
                    let _ = socket.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                }
                if matches!(mode, "json-flood" | "error-flood") {
                    let status = if mode == "json-flood" {
                        "200 OK"
                    } else {
                        "500 Internal Server Error"
                    };
                    // No length hint: the client must stop reading a streaming body itself.
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                    );
                    if socket.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    let chunk = [b' '; 8192];
                    loop {
                        if socket.write_all(&chunk).await.is_err() {
                            return;
                        }
                    }
                }
                let message: Value = serde_json::from_slice(&bytes).unwrap();
                if matches!(
                    mode,
                    "json-at-limit" | "json-over-limit" | "error-at-limit" | "error-over-limit"
                ) {
                    let error = mode.starts_with("error");
                    let limit = if error { ERROR_LIMIT } else { JSON_LIMIT };
                    let mut reply = if error {
                        json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32603,"message":"failed"}}).to_string()
                    } else {
                        json!({"jsonrpc":"2.0","id":message["id"],"result":{}}).to_string()
                    }.into_bytes();
                    reply.resize(limit + usize::from(mode.contains("over")), b' ');
                    let status = if error {
                        "500 Internal Server Error"
                    } else {
                        "200 OK"
                    };
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nMcp-Session-Id: fixture-session\r\nConnection: close\r\n\r\n",
                        reply.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&reply).await;
                    return;
                }
                if mode.starts_with("challenge-") {
                    let value = challenge_value(mode);
                    let header = format!(
                        "HTTP/1.1 403 Forbidden\r\nWWW-Authenticate: {value}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    return;
                }
                let (status, content_type, reply) = reply(mode, &message);
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nMcp-Session-Id: fixture-session\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (uri, task)
}

#[tokio::test]
async fn modern_and_legacy_servers_list_and_call_tools() {
    let dir = TempDir::new("mcp-http");
    for mode in ["modern", "legacy-json", "legacy-text"] {
        let (uri, task) = server(mode).await;
        let (connection, _service) = super::super::connect(
            "fx",
            crate::mcp::config::Server::Http {
                url: uri.to_string(),
                headers: std::collections::BTreeMap::new(),
            },
            &crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
            &Client::new(),
            &Store::new(dir.path().join("auth.json")),
            std::sync::Arc::default(),
            std::sync::Arc::default(),
        )
        .await
        .unwrap();
        assert_eq!(connection.catalog.tools.len(), 1);
        let result = connection
            .peer
            .call_tool(
                rmcp::model::CallToolRequestParams::new("echo")
                    .with_arguments(json!({"answer":42}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap(), json!({"answer":42}));
        connection.stop();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                connection
                    .peer
                    .call_tool(rmcp::model::CallToolRequestParams::new("echo"))
            )
            .await
            .unwrap()
            .is_err()
        );
        drop(connection);
        task.abort();
    }
}

fn reply(mode: &str, message: &Value) -> (&'static str, &'static str, String) {
    if mode.starts_with("sse") {
        (
            "200 OK",
            "text/event-stream",
            match mode {
                "sse-crlf" => "data: a\r\n\r\ndata: b\r\n\r\n",
                _ => "data: too large\n\n",
            }
            .to_owned(),
        )
    } else if message.get("id").is_none() {
        ("200 OK", "application/json", String::new())
    } else if message["method"] == "server/discover" && mode != "modern" {
        let reply = if mode == "legacy-text" {
            "initialize first".to_owned()
        } else {
            json!({"jsonrpc":"2.0","id":"server-error","error":{"code":-32600,"message":"initialize first"}}).to_string()
        };
        ("400 Bad Request", "application/json", reply)
    } else {
        let result = match message["method"].as_str().unwrap() {
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
            }
            "initialize" => {
                json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => {
                json!({"content":[],"structuredContent":message["params"]["arguments"]})
            }
            _ => json!({}),
        };
        (
            "200 OK",
            "application/json",
            json!({"jsonrpc":"2.0","id":message["id"],"result":result}).to_string(),
        )
    }
}

fn challenge_value(mode: &str) -> &str {
    match mode {
        "challenge-quoted" => "Bearer error=insufficient_scope, ScOpE=\"read write\"",
        "challenge-token" => "Bearer scope=read:data; error=insufficient_scope",
        _ => "Bearer scope=\"unterminated",
    }
}
