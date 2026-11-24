use std::{
    io::{Read as _, Write as _},
    sync::{Arc, Mutex},
};

use base64::Engine as _;
use serde_json::{Value, json};
use sha1::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::compat::TokioAsyncReadCompatExt as _;

use super::super::{Cache, Kind, Provider, Request, Tools, Transport, mock::completed, responses};
use crate::{
    auth::{Credential, Store},
    history::{Assistant, Image, Message, ToolResult},
};

type Frames = Arc<Mutex<Vec<(usize, Value)>>>;

fn provider(origin: &str) -> Provider {
    Provider {
        kind: Kind::Chatgpt(Box::new(reqwest::Url::parse(origin).unwrap())),
    }
}

async fn save_account(store: &Store, account: &str) {
    let credential: Credential =
        serde_json::from_value(json!({"type":"chatgpt", "access":"local-test-access",
        "refresh":"local-test-refresh", "expires_at":i64::MAX, "account_id":account,
        "residency":null, "installation_id":"5b0f3c1e-7a2d-4e8b-9c61-2f4d8a7e9b13"}))
        .unwrap();
    store
        .update(async |credentials| {
            credentials.providers.insert("chatgpt".into(), credential);
            Ok(())
        })
        .await
        .unwrap();
}

fn request<'a>(provider: &'a Provider, history: &'a [Message]) -> Request<'a> {
    Request {
        service_tier: Default::default(),
        provider,
        model: "m",
        instructions: "instructions",
        history,
        tools: Tools::Functions(&[]),
        reasoning_effort: None,
        summaries: false,
        verbosity: None,
        max_output_tokens: None,
        cache: Some(Cache {
            key: "key",
            window: 0,
        }),
    }
}

#[test]
fn service_tier_on_the_wire() {
    use crate::preferences::ServiceTier;
    let provider = provider("https://fixture.invalid/v1");
    let mut request = request(&provider, &[]);
    for tier in ServiceTier::ALL {
        request.service_tier = tier;
        let snapshot = super::super::prepare::Snapshot::new(&request);
        let body =
            responses::body(&snapshot.request(&[]), provider.endpoint().unwrap(), false).unwrap();
        assert_eq!(
            body.get("service_tier"),
            (tier != ServiceTier::Auto).then(|| json!(tier)).as_ref()
        );
    }
}

#[test]
fn service_tier_choices_follow_responses_protocol_and_catalog() {
    use super::super::{Custom, HttpProtocol, data_models};
    use crate::preferences::ServiceTier::{self, Auto, Flex};
    let custom = |protocol| {
        Provider::custom(
            "test",
            &Custom {
                protocol,
                url: "https://fixture.invalid/v1".into(),
                key: false,
            },
        )
        .unwrap()
    };
    let model = data_models(&json!({"data":[{"id":"m", "service_tiers":["flex", "turbo"]}]}))
        .unwrap()
        .remove(0);
    let responses = custom(HttpProtocol::Responses);
    assert_eq!(responses.service_tiers(None), ServiceTier::ALL);
    assert_eq!(responses.service_tiers(Some(&model)), [Auto, Flex]);
    assert!(
        custom(HttpProtocol::Chat)
            .service_tiers(Some(&model))
            .is_empty()
    );
}

#[test]
fn service_tier_is_limited_to_codex_catalog() {
    use crate::preferences::ServiceTier::{Auto, Flex, Priority};
    let provider = provider("http://localhost");
    let models = responses::codex_models(&json!({"models":[
        {"slug":"one","service_tiers":[{"id":"flex"},{"id":"unknown"}]},
        {"slug":"none", "service_tiers":[]}
    ]}))
    .unwrap();
    assert_eq!(provider.service_tiers(Some(&models[0])), [Auto, Flex]);
    assert_eq!(provider.service_tiers(Some(&models[1])), [Auto]);
    assert_eq!(provider.effective_tier(Some(&models[0]), Priority), Auto);
}

/// Serves Responses over WebSocket. Only the first connection sends a turn state.
async fn serve() -> (String, Frames) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let frames = Frames::default();
    let seen = Arc::clone(&frames);
    tokio::spawn(async move {
        let responses = Arc::new(Mutex::new(0));
        let mut connection = 0;
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut bytes = [0; 4096];
                let read = tcp.read(&mut bytes).await.unwrap();
                head.extend_from_slice(&bytes[..read]);
            }
            let head = String::from_utf8(head).unwrap();
            let key = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                .unwrap()
                .1
                .trim();
            let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
                format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"),
            ));
            let route = if connection == 0 {
                "x-codex-turn-state: route-A\r\n"
            } else {
                ""
            };
            tcp.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n{route}\r\n").as_bytes()).await.unwrap();
            let (mut sender, mut receiver) =
                soketto::connection::Builder::new(tcp.compat(), soketto::Mode::Server).finish();
            let seen = Arc::clone(&seen);
            let responses = Arc::clone(&responses);
            tokio::spawn(async move {
                let mut said = None;
                loop {
                    let mut data = Vec::new();
                    if receiver.receive_data(&mut data).await.is_err() {
                        return;
                    }
                    let frame: Value = serde_json::from_slice(&data).unwrap();
                    seen.lock().unwrap().push((connection, frame.clone()));
                    if let Some(item) = frame["input"].as_array().and_then(|input| input.last()) {
                        said = Some(item["content"][0]["text"].clone());
                    }
                    if said == Some(json!("lost")) && frame["previous_response_id"].is_string() {
                        let lost =
                            json!({"type":"error","error":{"code":"previous_response_not_found"}});
                        sender.send_text(lost.to_string()).await.unwrap();
                        sender.flush().await.unwrap();
                        continue;
                    }
                    let item = if said == Some(json!("tool")) {
                        json!({"type":"function_call", "id":"fc", "call_id":"call", "name":"python", "arguments":"{\"code\":\"1\"}", "status":"completed"})
                    } else {
                        json!({"type":"message", "id":"msg", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"done", "annotations":[]}]})
                    };
                    let output = if frame["generate"] == false {
                        Vec::new()
                    } else {
                        vec![item]
                    };
                    let mut events: Vec<_> = output.iter().map(|item| json!({"type":"response.output_item.done", "sequence_number":1, "output_index":0, "item":item})).collect();
                    let mut done = completed("m", &output);
                    let number = {
                        let mut responses = responses.lock().unwrap();
                        *responses += 1;
                        *responses
                    };
                    done["response"]["id"] = format!("resp{number}").into();
                    events.push(done);
                    for event in events {
                        sender.send_text(event.to_string()).await.unwrap();
                    }
                    sender.flush().await.unwrap();
                }
            });
            connection += 1;
        }
    });
    (origin, frames)
}

#[tokio::test]
async fn continuation_sends_only_appended_input_and_resets_on_any_doubt() {
    let (origin, frames) = serve().await;
    let provider = provider(&origin);
    let client = crate::http_client::new();
    let transport = Transport::default();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    save_account(&store, "account-A").await;
    let path = dir.path().join("image.png");
    std::fs::write(&path, crate::history::test_support::PNG).unwrap();
    let image = Message::Image(Image {
        number: 1,
        path: path.clone(),
        media_type: "image/png".into(),
        label: String::new(),
        byte_len: crate::history::test_support::PNG.len(),
    });
    let stream = async |history: &[Message], effort| -> Assistant {
        let mut request = request(&provider, history);
        request.reasoning_effort = effort;
        transport
            .stream(&client, &store, &request, |_: super::Delta<'_>| {}, None)
            .await
            .unwrap()
    };
    let frame = |index: usize| frames.lock().unwrap()[index].1.clone();
    let user = |text: &str| Message::User(text.into());
    let full = |history: &[Message], effort| {
        let mut request = request(&provider, history);
        request.reasoning_effort = effort;
        Value::Array(responses::input(&request).unwrap())
    };

    transport.begin_turn().await.unwrap();
    let mut history = vec![user("tool")];
    transport
        .prewarm(&client, &store, &request(&provider, &history))
        .await
        .unwrap();
    let called = stream(&history, None).await;
    history.extend([
        Message::Assistant(called),
        Message::ToolResult(ToolResult {
            activities: Default::default(),
            diffs: Vec::new(),
            notes: String::new(),
            call_id: "call".into(),
            output: "out".into(),
            is_error: false,
            jobs: Vec::new(),
            saved: None,
            cell: None,
        }),
    ]);
    let answered = stream(&history, None).await;
    assert_eq!(frame(0)["generate"], false);
    assert_eq!(frame(1)["previous_response_id"], "resp1");
    assert_eq!(frame(1)["input"], json!([]));
    assert_eq!(frame(2)["previous_response_id"], "resp2");
    let mut held = frame(0)["input"].as_array().unwrap().clone();
    held.push(json!({"type":"function_call", "id":"fc", "call_id":"call", "name":"python", "arguments":"{\"code\":\"1\"}", "status":"completed"}));
    held.extend(frame(2)["input"].as_array().unwrap().iter().cloned());
    assert_eq!(Value::Array(held), full(&history, None));

    history.extend([Message::Assistant(answered), user("lost")]);
    let recovered = stream(&history, None).await;
    assert_eq!(frame(3)["previous_response_id"], "resp3");
    history.extend([Message::Assistant(recovered), user("effort")]);
    stream(&history, Some("high")).await;
    let mut rewritten = vec![user("rewritten")];
    let rewrite = stream(&rewritten, Some("high")).await;
    rewritten.extend([Message::Assistant(rewrite), image, user("look")]);
    let looked = stream(&rewritten, Some("high")).await;
    assert_eq!(frame(7)["previous_response_id"], "resp6");
    assert_eq!(frame(7)["input"], full(&rewritten[2..], Some("high")));
    std::fs::write(&path, b"changed image bytes").unwrap();
    rewritten.extend([Message::Assistant(looked), user("again")]);
    let again = stream(&rewritten, Some("high")).await;
    let restarts = [
        (4, &history[..history.len() - 2], None),
        (5, &history[..], Some("high")),
        (6, &rewritten[..1], Some("high")),
        (8, &rewritten[..], Some("high")),
    ];
    for (index, history, effort) in restarts {
        assert!(
            frame(index).get("previous_response_id").is_none(),
            "{index}"
        );
        assert_eq!(frame(index)["input"], full(history, effort), "{index}");
    }
    assert!(
        frame(8)
            .to_string()
            .contains(&base64::engine::general_purpose::STANDARD.encode(b"changed image bytes"))
    );

    rewritten.extend([Message::Assistant(again), user("switched")]);
    save_account(&store, "account-B").await;
    stream(&rewritten, Some("high")).await;
    let frames = frames.lock().unwrap();
    let connections: Vec<_> = frames.iter().map(|(connection, _)| *connection).collect();
    assert_eq!(connections, [0, 0, 0, 0, 1, 1, 1, 1, 1, 2]);
    assert!(
        frames[..9]
            .iter()
            .all(|(_, frame)| frame["client_metadata"]["x-codex-turn-state"] == "route-A")
    );
    assert!(frame_clean(&frames[9].1));
}

fn frame_clean(frame: &Value) -> bool {
    frame.get("previous_response_id").is_none()
        && frame["client_metadata"].get("x-codex-turn-state").is_none()
}

/// Proxy variables are read once per process, so each case runs in a child.
#[test]
fn a_proxied_endpoint_never_dials_the_origin_directly() {
    let accept = |listener: std::net::TcpListener| {
        let heads = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&heads);
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut head = [0; 4096];
                let read = stream.read(&mut head).unwrap_or(0);
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head[..read]).to_lowercase());
                let _ = stream.write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        heads
    };
    let bind = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = bind().local_addr().unwrap();
    for case in ["proxied", "excluded", "broken"] {
        let (origin, proxy) = (bind(), bind());
        let origin_address = origin.local_addr().unwrap();
        let proxy_url = match case {
            "broken" => format!("http://{closed}"),
            _ => format!("http://{}", proxy.local_addr().unwrap()),
        };
        let (origin_heads, proxy_heads) = (accept(origin), accept(proxy));
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "provider::chatgpt::tests::proxy_child",
                "--ignored",
            ])
            .env("BART_TEST_ORIGIN", format!("http://{origin_address}"));
        for name in ["ALL_PROXY", "all_proxy", "REQUEST_METHOD"] {
            child.env_remove(name);
        }
        for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
            child.env(name, &proxy_url);
        }
        let excluded = if case == "excluded" { "127.0.0.1" } else { "" };
        child.env("NO_PROXY", excluded).env("no_proxy", excluded);
        assert!(child.status().unwrap().success(), "{case}");
        let origin_heads = origin_heads.lock().unwrap();
        let proxy_heads = proxy_heads.lock().unwrap();
        match case {
            "proxied" => {
                assert!(origin_heads.is_empty());
                assert_eq!(proxy_heads.len(), 1);
                assert!(
                    proxy_heads[0]
                        .starts_with(&format!("post http://{origin_address}/responses http/1.1"))
                );
            }
            "excluded" => {
                assert!(proxy_heads.is_empty());
                assert!(!origin_heads.is_empty());
                assert!(
                    origin_heads
                        .iter()
                        .all(|head| head.contains("upgrade: websocket"))
                );
            }
            _ => assert!(origin_heads.is_empty() && proxy_heads.is_empty()),
        }
    }
}

#[tokio::test]
#[ignore = "run by a_proxied_endpoint_never_dials_the_origin_directly"]
async fn proxy_child() {
    let Ok(origin) = std::env::var("BART_TEST_ORIGIN") else {
        return;
    };
    let provider = provider(&origin);
    let history = [Message::User("hello".into())];
    let request = request(&provider, &history);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    save_account(&store, "account").await;
    let transport = Transport::default();
    let client = crate::http_client::new();
    let _ = transport.prewarm(&client, &store, &request).await;
    let _ = transport
        .stream(&client, &store, &request, |_: super::Delta<'_>| {}, None)
        .await;
}

#[tokio::test]
async fn websocket_error_event_with_context_overflow_classifies_as_overflow() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut bytes = [0; 4096];
            let read = tcp.read(&mut bytes).await.unwrap();
            head.extend_from_slice(&bytes[..read]);
        }
        let head = String::from_utf8(head).unwrap();
        let key = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
            .unwrap()
            .1
            .trim();
        let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(format!(
            "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        )));
        tcp.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
        let (mut sender, mut receiver) =
            soketto::connection::Builder::new(tcp.compat(), soketto::Mode::Server).finish();
        let mut data = Vec::new();
        receiver.receive_data(&mut data).await.unwrap();
        let overflow = json!({
            "type": "error",
            "status": 400,
            "error": {
                "code": "context_length_exceeded",
                "message": "Your input exceeds the context window of this model."
            }
        });
        sender.send_text(overflow.to_string()).await.unwrap();
        sender.flush().await.unwrap();
    });

    let provider = provider(&origin);
    let history = [Message::User("hello".into())];
    let request = request(&provider, &history);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    save_account(&store, "account").await;
    let transport = Transport::default();
    let error = transport
        .stream(
            &crate::http_client::new(),
            &store,
            &request,
            |_: super::Delta<'_>| {},
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, super::super::Error::ContextOverflow(_)),
        "expected ContextOverflow, got {error:?}"
    );
}

#[tokio::test]
async fn websocket_upgrade_refusal_without_content_length_and_many_headers_preserves_error_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let resets_at = super::super::unix_now() + 3600;
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut bytes = [0; 4096];
            let read = tcp.read(&mut bytes).await.unwrap();
            head.extend_from_slice(&bytes[..read]);
        }
        let body = json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "usage limit hit",
                "resets_at": resets_at
            }
        })
        .to_string();
        let mut response = String::from("HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n");
        for i in 0..70 {
            response.push_str(&format!("X-Extra-{i}: value\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&body);
        tcp.write_all(response.as_bytes()).await.unwrap();
        tcp.shutdown().await.unwrap();
    });

    let provider = provider(&origin);
    let history = [Message::User("hello".into())];
    let request = request(&provider, &history);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    save_account(&store, "account").await;
    let transport = Transport::default();
    let error = transport
        .stream(
            &crate::http_client::new(),
            &store,
            &request,
            |_: super::Delta<'_>| {},
            None,
        )
        .await
        .unwrap_err();
    match error {
        super::super::Error::Http {
            status,
            message,
            retry,
        } => {
            assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
            assert!(
                message.contains("usage limit hit"),
                "expected preserved error message, got: {message}"
            );
            assert!(
                matches!(retry, super::super::Retry::Reset(_)),
                "expected Retry::Reset, got: {retry:?}"
            );
        }
        other => panic!("expected Error::Http with preserved body, got {other:?}"),
    }
}

#[tokio::test]
async fn truncated_websocket_upgrade_refusal_does_not_trigger_context_overflow() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut bytes = [0; 4096];
            let read = tcp.read(&mut bytes).await.unwrap();
            head.extend_from_slice(&bytes[..read]);
        }
        // Valid JSON prefix that matches context_length_exceeded, but truncated
        // well short of the declared Content-Length.
        let body = r#"{"error":{"code":"context_length_exceeded"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len() + 100
        );
        tcp.write_all(response.as_bytes()).await.unwrap();
        tcp.shutdown().await.unwrap();
    });

    let provider = provider(&origin);
    let history = [Message::User("hello".into())];
    let request = request(&provider, &history);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    save_account(&store, "account").await;
    let transport = Transport::default();
    let error = transport
        .stream(
            &crate::http_client::new(),
            &store,
            &request,
            |_: super::Delta<'_>| {},
            None,
        )
        .await
        .unwrap_err();
    match error {
        super::super::Error::Http {
            status, message, ..
        } => {
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
            assert!(message.contains("[incomplete HTTP body]"), "{message}");
        }
        other => panic!("truncated refusal must not classify as ContextOverflow: {other:?}"),
    }
}
