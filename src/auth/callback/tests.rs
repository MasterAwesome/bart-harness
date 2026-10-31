use super::*;

#[tokio::test]
async fn callback_acts_only_with_matching_state_and_a_code() {
    for (target, expected) in [
        ("/favicon.ico", Some("fallback")),
        ("/auth/callback?code=abc&state=s1", Some("abc")),
        (
            "/auth/callback?code=a%2Bb&state=s1&extra=value",
            Some("a+b"),
        ),
        ("/auth/callback?code=abc&state=forged", Some("fallback")),
        (
            "/auth/callback?error=access_denied&state=forged",
            Some("fallback"),
        ),
        ("/auth/callback?state=s1", None),
        (
            "/auth/callback?error=access_denied&error_description=User%20cancelled&state=s1",
            None,
        ),
        ("/other?code=abc&state=s1", Some("fallback")),
    ] {
        let deadline = time::Instant::now() + Duration::from_secs(5);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiving = tokio::spawn(async move {
            receive(std::slice::from_ref(&listener), "/auth/callback", "s1").await
        });
        let mut browser = TcpStream::connect(address).await.unwrap();
        browser
            .write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        time::timeout_at(deadline, browser.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        if expected == Some("fallback") {
            let mut browser = TcpStream::connect(address).await.unwrap();
            browser
                .write_all(b"GET /auth/callback?code=fallback&state=s1 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
        }
        let received = time::timeout_at(deadline, receiving)
            .await
            .unwrap()
            .unwrap();
        match expected {
            Some(code) => assert_eq!(received.unwrap().code, code, "{target}"),
            None => assert!(
                matches!(received, Err(Error::Login(problem)) if !problem.is_empty()),
                "{target}"
            ),
        }
    }
}
