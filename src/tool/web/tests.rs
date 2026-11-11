use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::provider::{
    Custom, Protocol,
    mock::{Reply, serve_catalog},
};

fn reply(chunks: Vec<Value>) -> Reply {
    Reply {
        status: 200,
        authorization: None,
        delay: Duration::ZERO,
        chunks,
        ends: false,
    }
}

fn messages(blocks: &[Value], deltas: &[(u64, Value)], stop: &str) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg","model":"m","usage":{"input_tokens":1,"output_tokens":1}}}),
    ];
    for (index, block) in blocks.iter().enumerate() {
        let index = index as u64;
        events.push(json!({"type":"content_block_start","index":index,"content_block":block}));
        events.extend(
            deltas.iter().filter(|(at, _)| *at == index).map(
                |(_, delta)| json!({"type":"content_block_delta","index":index,"delta":delta}),
            ),
        );
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":stop}}));
    events.push(json!({"type":"message_stop"}));
    events
}

async fn search_on(
    protocol: Protocol,
    replies: Vec<Reply>,
) -> (Result<String, String>, Vec<Value>) {
    let dir = crate::tool::test_support::TempDir::new("web-search-protocols");
    let (provider, requests, _, _) = serve_catalog(
        dir.path(),
        replies,
        json!({"data": [{"id": "m", "max_tokens": 32_000}], "has_more": false}),
    )
    .await;
    let provider = Provider::custom(
        provider.name(),
        &Custom {
            protocol,
            url: provider.endpoint().unwrap().to_string(),
            key: false,
        },
    )
    .unwrap();
    let route = Route {
        client: Client::new(),
        store: Store::new(dir.path().join("auth.json")),
        provider,
        model: "m".into(),
        effort: None,
        diagnostics: None,
    };
    let answer = search_one(&route, "q").await;
    let requests = requests.lock().unwrap().clone();
    (answer, requests)
}

fn assert_claims(answer: &str, sources: &[(&str, &str)]) {
    for (url, claim) in sources {
        let entry = answer
            .split_once(url)
            .unwrap()
            .1
            .split("https://")
            .next()
            .unwrap();
        assert!(entry.contains(claim), "{answer}");
    }
}

#[tokio::test]
async fn anthropic_search_resumes_paused_turns_unchanged_and_reports_failures() {
    let result = |url: &str| json!({"type":"web_search_result","url":url,"title":"R","encrypted_content":"opaque"});
    let paused_blocks = [
        json!({"type":"text","text":""}),
        json!({"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}),
        json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[result("https://b.example/")]}),
    ];
    let paused = messages(
        &paused_blocks,
        &[(
            1,
            json!({"type":"input_json_delta","partial_json":"{\"query\":\"q\"}"}),
        )],
        "pause_turn",
    );
    let answered = messages(
        &[
            json!({"type":"text","text":""}),
            json!({"type":"text","text":""}),
        ],
        &[
            (0, json!({"type":"text_delta","text":"A."})),
            (
                0,
                json!({"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://a.example/","title":"A","encrypted_index":"i"}}),
            ),
            (1, json!({"type":"text_delta","text":"B."})),
            (
                1,
                json!({"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://b.example/","title":"B","encrypted_index":"j"}}),
            ),
        ],
        "end_turn",
    );
    let (answer, requests) =
        search_on(Protocol::Messages, vec![reply(paused), reply(answered)]).await;
    let answer = answer.unwrap();
    assert!(answer.starts_with("A.B."));
    assert_claims(
        &answer,
        &[("https://a.example/", "A."), ("https://b.example/", "B.")],
    );
    assert!(requests[0].to_string().contains("web_search_20250305"));
    let mut replayed = paused_blocks.to_vec();
    replayed[1]["input"] = json!({"query":"q"});
    assert_eq!(
        requests[1]["messages"][1]["content"],
        json!(replayed),
        "paused content returns unchanged"
    );

    let failed = messages(
        &[
            json!({"type":"web_search_tool_result","tool_use_id":"s","content":{"type":"web_search_tool_result_error","error_code":"too_many_requests"}}),
        ],
        &[],
        "end_turn",
    );
    let (answer, _) = search_on(Protocol::Messages, vec![reply(failed)]).await;
    assert!(answer.unwrap().contains("too_many_requests"));
    let mut unsigned = result("https://a.example/");
    unsigned
        .as_object_mut()
        .unwrap()
        .remove("encrypted_content");
    let malformed = messages(
        &[json!({"type":"web_search_tool_result","tool_use_id":"s","content":[unsigned]})],
        &[],
        "end_turn",
    );
    let uncited = messages(
        &[json!({"type":"text","text":""})],
        &[(
            0,
            json!({"type":"citations_delta","citation":{"type":"web_search_result_location"}}),
        )],
        "pause_turn",
    );
    for malformed in [malformed, uncited] {
        assert!(
            search_on(Protocol::Messages, vec![reply(malformed)])
                .await
                .0
                .is_err()
        );
    }
}

#[tokio::test]
async fn batch_runs_concurrently_and_keeps_failures_with_their_queries() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    let dir = crate::tool::test_support::TempDir::new("web-search-batch");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut posts = Vec::new();
        while posts.len() < 2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let end = loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|x| x == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
            if headers.starts_with("get ") {
                let body = r#"{"data":[{"id":"m","max_tokens":32000}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                continue;
            }
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while request.len() < end + length {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
            }
            let body = String::from_utf8(request[end..].to_vec()).unwrap();
            posts.push((socket, body));
        }
        for (mut socket, request) in posts.into_iter().rev() {
            let (status, body) = if request.contains("second query") {
                (500, "failure".to_owned())
            } else {
                (
                    200,
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"index":0,"delta":{"content":"first answer"}}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
                    ),
                )
            };
            let response = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let provider = Provider::custom(
        "mock",
        &Custom {
            protocol: Protocol::Chat,
            url,
            key: false,
        },
    )
    .unwrap();
    let route = Route {
        client: Client::new(),
        store: Store::new(dir.path().join("auth.json")),
        provider,
        model: "m".into(),
        effort: None,
        diagnostics: None,
    };
    let live = std::sync::Arc::new(crate::tool::Live::default());
    let run = live.start("python", "", None);
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        search(
            &route,
            &["first query".into(), "second query".into()],
            run.progress(),
        ),
    )
    .await
    .expect("both requests must be in flight");
    server.await.unwrap();
    let (first, second) = output
        .split_once("[Bart web_search: second query]")
        .unwrap();
    assert!(
        first.contains("[Bart web_search: first query]") && first.contains("first answer"),
        "{output}"
    );
    assert!(second.contains("failed;"), "{output}");
}
