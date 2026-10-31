use crate::{
    auth::{self, Credential, Store},
    provider::{self, Custom, Protocol, Provider},
};
use reqwest::Client;
async fn seed(store: &Store, provider_name: &str) {
    store
        .update(async |credentials| {
            credentials.providers.insert(
                provider_name.into(),
                Credential::ApiKey {
                    key: auth::api_key("provider-secret").unwrap(),
                },
            );
            credentials.mcp.insert(
                "docs".into(),
                auth::SecretValue(serde_json::json!({
                    "access_token": "mcp-secret"
                })),
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn provider_and_mcp_logout_never_cross_namespaces() {
    let dir = crate::tool::test_support::TempDir::new("auth-namespaces");
    let store = Store::new(dir.path().join("auth.json"));
    for provider_name in ["docs", "mcp-docs"] {
        seed(&store, provider_name).await;
        assert!(super::logout("docs", &store).await.unwrap());
        let provider = Provider::custom(
            provider_name,
            &Custom {
                protocol: Protocol::Chat,
                url: "https://example.invalid/v1".into(),
                key: true,
            },
        )
        .unwrap();
        assert!(matches!(
            provider::credential(&store, &Client::new(), &provider)
                .await
                .unwrap(),
            Some(Credential::ApiKey { .. })
        ));
        crate::logout(dir.path(), &store, provider_name)
            .await
            .unwrap();
    }
}

async fn authorized(store: &Store, endpoint: &str) -> super::Authorized {
    use rmcp::transport::auth::{AuthorizationManager, AuthorizationMetadata};
    let mut manager = AuthorizationManager::new(endpoint).await.unwrap();
    manager.with_client(crate::http_client::oauth()).unwrap();
    let mut metadata = AuthorizationMetadata::default();
    metadata.authorization_endpoint = endpoint.into();
    metadata.token_endpoint = endpoint.into();
    manager.set_metadata(metadata);
    let tokens = super::Tokens::new(store, "docs");
    manager.set_credential_store(tokens.clone());
    assert!(manager.initialize_from_store().await.unwrap());
    super::Authorized {
        http: super::super::http::Http(Client::new()),
        manager: std::sync::Arc::new(tokio::sync::Mutex::new(manager)),
        tokens,
    }
}

async fn challenge_server() -> (
    std::sync::Arc<str>,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri: std::sync::Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
    let (send, received) = tokio::sync::mpsc::unbounded_channel();
    let service = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8(bytes).unwrap();
            let token = if request.contains("Bearer access-B") {
                "B"
            } else {
                "A"
            };
            send.send(token).unwrap();
            let response = if token == "A" {
                "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer resource_metadata=\"http://localhost/metadata\"\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            };
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (uri, received, service)
}

fn initialized() -> rmcp::model::ClientJsonRpcMessage {
    serde_json::from_value(
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .unwrap()
}

async fn dropped_mcp_refresh(proactive: bool) {
    use crate::auth::refresh;
    use rmcp::transport::streamable_http_client::StreamableHttpClient as _;
    use std::{collections::HashMap, sync::atomic::Ordering};
    let _serial = refresh::mock::SERIAL.lock().await;
    let server = refresh::mock::Server::new(r#"{"access_token":"access-B","refresh_token":"B","token_type":"Bearer","expires_in":3600}"#).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    store.update(async |all| {
        all.mcp.insert("docs".into(), auth::SecretValue(serde_json::json!({
            "client_id":"fixture", "token_response": {
                "access_token":"access-A", "refresh_token":"A", "token_type":"Bearer", "expires_in":3600
            }, "token_received_at": if proactive {0} else {std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()}
        })));
        Ok(())
    }).await.unwrap();
    let (uri, mut received, service) = challenge_server().await;
    let client = authorized(&store, &server.url).await;
    let competing = authorized(&store, &server.url).await;
    let caller = {
        let (client, uri) = (client.clone(), uri.clone());
        tokio::spawn(async move {
            client
                .post_message(uri, initialized(), None, None, HashMap::new())
                .await
        })
    };
    server.started.notified().await;
    if !proactive {
        assert_eq!(received.recv().await, Some("A"));
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let next = {
        let uri = uri.clone();
        tokio::spawn(async move {
            competing
                .post_message(uri, initialized(), None, None, HashMap::new())
                .await
        })
    };
    let draining = tokio::spawn(refresh::drain());
    tokio::task::yield_now().await;
    assert!(!draining.is_finished());
    server.release.notify_one();
    draining.await.unwrap();
    next.await.unwrap().unwrap();
    let mut sent = Vec::new();
    while let Ok(token) = received.try_recv() {
        sent.push(token);
    }
    // The competing request may first send A while the refresh is in flight.
    assert_eq!(sent.last(), Some(&"B"));
    assert_eq!(
        sent.iter().filter(|token| **token == "B").count(),
        1,
        "cancelled service must not retry its request"
    );
    assert_eq!(
        store.load().unwrap().mcp["docs"].0["token_response"]["refresh_token"],
        "B"
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    let lock = store.lock().await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), client.token(None))
            .await
            .expect("a usable token is sent without the credential lock")
            .unwrap(),
        "access-B"
    );
    let pending = tokio::spawn(async move { client.token(Some("access-B".into())).await });
    tokio::task::yield_now().await;
    tokio::time::timeout(std::time::Duration::from_secs(1), refresh::drain())
        .await
        .unwrap();
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    drop(lock);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    service.abort();
}

#[tokio::test]
async fn dropped_proactive_refresh_is_saved_without_running_cancelled_service() {
    dropped_mcp_refresh(true).await;
}

#[tokio::test]
async fn dropped_401_refresh_is_saved_without_retrying_cancelled_service() {
    dropped_mcp_refresh(false).await;
}
