use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::Notify,
};
pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub(crate) struct Server {
    pub(crate) url: String,
    pub(crate) started: Arc<Notify>,
    pub(crate) release: Arc<Notify>,
    pub(crate) requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub(crate) async fn new(response: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let (seen, go, count) = (started.clone(), release.clone(), requests.clone());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let len: usize = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|n| n.parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let index = count.fetch_add(1, Ordering::SeqCst);
                assert!(String::from_utf8_lossy(&bytes).contains("refresh_token=A"));
                if index == 0 {
                    seen.notify_one();
                    go.notified().await;
                }
                let (status, body) = if index == 0 {
                    ("200 OK", response)
                } else {
                    ("400 Bad Request", r#"{"error":"invalid_grant"}"#)
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        Self {
            url,
            started,
            release,
            requests,
            task,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
