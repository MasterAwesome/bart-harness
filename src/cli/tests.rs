use super::*;

fn parsed(line: &[&str]) -> Result<Invocation, clap::Error> {
    parse(["bart"].iter().chain(line).map(std::ffi::OsString::from))
}

#[test]
fn the_command_line_refuses_ambiguity_and_keeps_prompts_verbatim() {
    for line in [
        &["--model", "a", "--model", "b"][..],
        &["--resume", "--session", "x"],
        &["--no-network"],
        &["--model"],
        &["--model", ""],
        &["--prompt", "  "],
        &["--approval", "sometimes"],
        &["--surprise"],
        &["login", "chatgpt", "--url", "http://x"],
        &["login", "chat:local"],
        &["login", "work", "--device"],
        &["mcp", "add", "x"],
        &["mcp", "add", "x", "--url", "http://x", "--", "server"],
        &[
            "mcp",
            "add",
            "x",
            "--header",
            "X-Key: value",
            "--",
            "server",
        ],
        &["mcp", "add", "x", "-H", "X-Key: value", "--", "server"],
        &[
            "mcp",
            "add",
            "x",
            "--bearer-token-env",
            "TOKEN",
            "--",
            "server",
        ],
        &["mcp", "add", "x", "--url", "http://x", "--env", "K=v"],
        &["models", "a", "b"],
    ] {
        assert!(parsed(line).is_err(), "{line:?}");
    }
    for line in [
        &["mcp", "add", "x", "--", "server"][..],
        &["mcp", "add", "x", "--env", "K=v", "--", "server"],
        &[
            "mcp",
            "add",
            "x",
            "--",
            "server",
            "--header",
            "X-Key: value",
        ],
        &[
            "mcp",
            "add",
            "x",
            "--url",
            "http://x",
            "--header",
            "X-Key: value",
        ],
        &[
            "mcp",
            "add",
            "x",
            "--url",
            "http://x",
            "--bearer-token-env",
            "TOKEN",
        ],
    ] {
        assert!(parsed(line).is_ok(), "{line:?}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        let invalid = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(parse(["bart".into(), "--prompt".into(), invalid.clone()]).is_err());
        assert!(parse(["bart".into(), "mcp".into(), "import".into(), invalid]).is_err());
    }
    let Ok(Invocation::Chat(chat)) = parsed(&["--prompt", "--model", "--session", "x"]) else {
        panic!("a prompt that starts with a dash");
    };
    assert_eq!(chat.prompt.as_deref(), Some("--model"));
    assert_eq!(chat.resume, Resume::Session("x".into()));
    #[cfg(target_os = "linux")]
    {
        let Ok(Invocation::Chat(chat)) = parsed(&["--jail", "--no-network"]) else {
            panic!("a jail without network");
        };
        assert_eq!(chat.jail, Some(Network::Off));
    }
    assert_eq!(
        parsed(&["login", "chat:local", "--url", "http://h/v1", "--no-key"]).unwrap(),
        Invocation::Login(Login::Custom {
            name: "local".into(),
            config: Custom {
                protocol: Protocol::Chat,
                url: "http://h/v1".into(),
                key: false,
            },
        })
    );
}

#[test]
fn keeping_background_jobs_requires_a_persistent_unsandboxed_prompt() {
    for args in [
        vec!["--keep-background-jobs"],
        vec!["--json", "--keep-background-jobs"],
        vec!["-p", "test", "--keep-background-jobs", "--temporary"],
        vec!["-p", "test", "--keep-background-jobs", "--jail"],
    ] {
        assert!(parsed(&args).is_err(), "{args:?}");
    }
    let Invocation::Chat(chat) = parsed(&["-p", "test", "--keep-background-jobs"]).unwrap() else {
        panic!()
    };
    assert!(chat.keep_background_jobs);
    assert_eq!(chat.prompt.as_deref(), Some("test"));
}
