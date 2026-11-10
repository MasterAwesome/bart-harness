use std::sync::Arc;

use reqwest::Client;

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
                let message: Value = serde_json::from_slice(&bytes).unwrap();
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
        let (connection, _service) = super::connect(
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
    if message.get("id").is_none() {
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
