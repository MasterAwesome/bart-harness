use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn redirects_keep_all_credentials_at_the_original_origin() {
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink_url = format!("http://{}/sink", sink.local_addr().unwrap());
    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", origin.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let requests = captured.clone();
    let serving = tokio::spawn(async move {
        loop {
            let (mut socket, _) = origin.accept().await.unwrap();
            let mut data = Vec::new();
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                if count == 0 {
                    break;
                }
                data.extend_from_slice(&bytes[..count]);
            }
            let request = String::from_utf8(data).unwrap();
            let path = request.split_whitespace().nth(1).unwrap();
            let response = match path {
                "/local" => "HTTP/1.1 307 Temporary Redirect\r\nLocation: /done\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                "/loop" => "HTTP/1.1 307 Temporary Redirect\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                "/foreign" => format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {sink_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                "/done" => {
                    requests.lock().unwrap().push(request);
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_owned()
                }
                _ => panic!("unexpected path"),
            };
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = new();
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        for path in ["local", "foreign"] {
            let result = client
                .request(method.clone(), format!("{base}/{path}"))
                .header("authorization", "Bearer synthetic")
                .header("x-api-key", "synthetic-anthropic")
                .header("x-goog-api-key", "synthetic-gemini")
                .header("x-custom-mcp-secret", "synthetic-mcp")
                .send()
                .await;
            if path == "local" {
                assert!(result.unwrap().status().is_success());
            } else {
                assert!(result.unwrap_err().is_redirect());
            }
        }
    }
    assert!(
        client
            .get(format!("{base}/loop"))
            .send()
            .await
            .unwrap_err()
            .is_redirect()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), sink.accept())
            .await
            .is_err()
    );
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for request in &*seen {
        for value in [
            "Bearer synthetic",
            "synthetic-anthropic",
            "synthetic-gemini",
            "synthetic-mcp",
        ] {
            assert!(request.contains(value));
        }
    }
    serving.abort();
}

#[tokio::test]
async fn oauth_token_request_has_a_total_deadline() {
    use tokio::io::AsyncReadExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let (send, started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n")
            .await
            .unwrap();
        std::future::pending::<()>().await;
        drop(socket);
    });
    let request = tokio::spawn(async move {
        let response = super::oauth().post(url).send().await?;
        let _ = send.send(());
        response.bytes().await
    });
    started.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    assert!(request.await.unwrap().unwrap_err().is_timeout());
    server.abort();
}
