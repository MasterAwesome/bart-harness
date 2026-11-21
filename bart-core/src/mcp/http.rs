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
                    if socket.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
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
        let mut io = super::ServerIo::new(Default::default());
        let connection = super::connect(
            "fx",
            crate::mcp::config::Server::Http {
                url: uri.to_string(),
                headers: std::collections::BTreeMap::new(),
            },
            &crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
            &Client::new(),
            &Store::new(dir.path().join("auth.json")),
            std::sync::Arc::default(),
            &mut io,
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
        tokio::time::timeout(
            crate::supervise::CLEANUP,
            io.service.take().unwrap().waiting(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(connection);
        task.abort();
    }
}

#[tokio::test]
async fn replace_after_completed_http_failure() {
    use super::{Config, Registry, config::Entry};
    let dir = TempDir::new("http-retry");
    let (denied, denial) = server("denied").await;
    let (healthy, replacement) = server("modern").await;
    let config =
        Config::parse(&json!({"mcpServers":{"fx":{"url":denied.as_ref()}}}).to_string()).unwrap();
    let registry = Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let error = registry.tools(Some("fx"), None).await.unwrap_err();
        assert!(error.contains("403"), "{error}");
        registry
            .apply("fx", &Entry::new(json!({"url":healthy.as_ref()})))
            .await
            .unwrap();
        assert!(
            registry
                .tools(Some("fx"), None)
                .await
                .unwrap()
                .contains("echo")
        );
        registry.close().await.unwrap();
    })
    .await
    .unwrap();
    denial.abort();
    replacement.abort();
}

#[tokio::test]
async fn held_http_handshake_retains_cleanup_through_close_and_replacement() {
    tokio::join!(held_handshake(true), held_handshake(false));
}

async fn held_handshake(close: bool) {
    use super::{Config, Registry, config::Entry};
    let dir = TempDir::new("mcp-held-handshake");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry = json!({"url":format!("http://{}/mcp",listener.local_addr().unwrap())});
    let config = Config::parse(&json!({"mcpServers":{"fx":entry}}).to_string()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        let registry = Registry::start(
            &config,
            crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
            &Client::new(),
            &Store::new(dir.path().join("auth.json")),
        );
        let socket = held_discover(&listener).await;
        let (healthy, server_task) = server("modern").await;
        let replacement = Entry::new(json!({"url":healthy.as_ref()}));
        let outcome = if close {
            registry.close().await
        } else {
            registry.apply("fx", &replacement).await
        };
        match outcome {
            Ok(()) => {
                // No reply was sent. Success is only possible if the client
                // already closed the held request, not just requested cancellation.
                assert!(
                    matches!(socket.get_ref().try_read(&mut [0]), Ok(0)),
                    "startup POST still open after settlement"
                );
            }
            Err(_) => {
                if !close {
                    assert!(
                        matches!(registry.states()[0].1, super::State::Failed(_)),
                        "unsettled server was replaced"
                    );
                }
                if close {
                    drop(socket);
                } else {
                    // Both retries see the same unsettled owner. Once it exits,
                    // only one may install its replacement under that permission.
                    let (first, second, ()) = tokio::join!(biased;
                        registry.apply("fx", &replacement),
                        registry.apply("fx", &replacement),
                        async { drop(socket) },
                    );
                    assert_ne!(first.is_ok(), second.is_ok(), "{first:?}, {second:?}");
                }
            }
        }
        if !close {
            assert!(
                registry
                    .tools(Some("fx"), None)
                    .await
                    .unwrap()
                    .contains("echo")
            );
        }
        registry.close().await.unwrap();
        server_task.abort();
    })
    .await
    .unwrap();
}

async fn held_discover(listener: &TcpListener) -> BufReader<tokio::net::TcpStream> {
    let (socket, _) = listener.accept().await.unwrap();
    let mut socket = BufReader::new(socket);
    let mut length = None;
    loop {
        let mut line = String::new();
        assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.unwrap()];
    socket.read_exact(&mut body).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["method"],
        "server/discover"
    );
    socket
}

#[tokio::test]
async fn toggle_reports_saved_choice_and_notifies_observers_while_shutdown_is_pending() {
    use super::{Config, Registry};
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new("mcp-toggle-pending");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry = json!({"url":format!("http://{}/mcp",listener.local_addr().unwrap())});
    let config = Config::parse(&json!({"mcpServers":{"fx":entry}}).to_string()).unwrap();
    let file = super::path(dir.path());
    config.save(&file).unwrap();
    let registry = Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        let socket = held_discover(&listener).await;
        let changes = registry.changes();
        // Configuration replacement succeeds, but opening the directory for sync fails.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o300)).unwrap();
        let result = super::toggle("fx", dir.path(), &registry).await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            Config::load(&file)
                .unwrap()
                .entries()
                .next()
                .unwrap()
                .1
                .disabled()
        );
        match result {
            Err(error) => {
                assert!(
                    error.contains("disabled in configuration")
                        && error.contains("shutdown unconfirmed")
                        && error.contains("durability"),
                    "{error}"
                );
                assert!(matches!(registry.states()[0].1, super::State::Failed(_)));
            }
            Ok((false, Some(warning))) => {
                assert!(warning.contains("durability"));
                assert!(matches!(socket.get_ref().try_read(&mut [0]), Ok(0)));
                assert!(matches!(registry.states()[0].1, super::State::Disabled(_)));
            }
            other => panic!("{other:?}"),
        }
        assert!(changes.has_changed().unwrap());
        drop(socket);
        registry.close().await.unwrap();
    })
    .await
    .unwrap();
}

fn reply(mode: &str, message: &Value) -> (&'static str, &'static str, String) {
    if mode == "denied" {
        ("403 Forbidden", "text/plain", String::new())
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
