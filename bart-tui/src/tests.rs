use super::*;
use std::fs;

fn command(line: &[&str]) -> Invocation {
    cli::parse(["bart"].iter().chain(line).map(std::ffi::OsString::from)).unwrap()
}

#[tokio::test]
async fn headless_runs_refuse_manual_approval_before_starting() {
    let dir = crate::tool::test_support::TempDir::new("headless-manual");
    let refuse = async |line: &[&str]| {
        let Invocation::Chat(options) = command(line) else {
            panic!("chat options");
        };
        let store = Store::new(dir.path().join("auth.json"));
        let error = chat(dir.path(), store, Client::new(), options, None)
            .await
            .unwrap_err();
        assert!(error.contains("manual approval"), "{error}");
    };
    refuse(&["--prompt", "go", "--approval", "manual"]).await;
    fs::write(
        dir.path().join("settings.json"),
        r#"{"permissions":{"approval":{"mode":"manual"}}}"#,
    )
    .unwrap();
    refuse(&["--prompt", "go"]).await;
}
