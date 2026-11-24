use std::process::Command;

#[test]
fn providers_stream_tool_round_trips_over_their_real_boundaries() {
    for fixture in ["claude.py", "http_protocols.py"] {
        let output = Command::new("python3")
            .arg(format!(
                "{}/tests/fixtures/{fixture}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .arg(env!("CARGO_BIN_EXE_bart"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{fixture}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn key_command_login_verifies_before_writing_and_stores_the_binding_not_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config/bart");
    let auth = config.join("auth.json");
    let providers = config.join("providers.json");
    let secret_path = dir.path().join("external-key");
    let secret = "secret-from-external-helper";
    std::fs::write(&secret_path, secret).unwrap();
    let key_file = secret_path.to_str().unwrap();
    let bart = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_bart"))
            .args(args)
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env_remove("HOME")
            .env("BART_TEST_KEY_HELPER", "login")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let login = |target: &str, new_url: &str, command: &[&str]| {
        let mut args = vec!["login", target];
        if target.contains(':') {
            args.extend(["--url", new_url]);
        }
        args.extend(["--key-command", "--"]);
        args.extend(command);
        bart(&args)
    };
    let original_url = "http://127.0.0.1:1/v1";
    let failure = ["sh", "-c", "echo vault is locked >&2; exit 2"];
    let (ok, stderr) = login("chat:gw", original_url, &failure);
    assert!(
        !ok && stderr.contains("exited with status 2") && !stderr.contains("vault is locked"),
        "{stderr}"
    );
    assert!(!auth.exists() && !providers.exists());

    let helper = [
        "sh",
        "-c",
        r#"[ "$PWD" = / ] && [ "$BART_TEST_KEY_HELPER" = login ] && cat "$1""#,
        "sh",
        key_file,
    ];
    let (ok, stderr) = login("chat:gw", original_url, &helper);
    assert!(ok && !stderr.contains(secret), "{stderr}");
    let saved_auth = std::fs::read_to_string(&auth).unwrap();
    assert!(!saved_auth.contains(secret));
    let stored: serde_json::Value = serde_json::from_str(&saved_auth).unwrap();
    assert_eq!(
        stored["providers"]["gw"],
        serde_json::json!({"type":"command", "endpoint":original_url, "argv":helper})
    );

    let (ok, stderr) = bart(&["login", "chat:open", "--url", original_url, "--no-key"]);
    assert!(ok, "{stderr}");
    let saved_providers = std::fs::read(&providers).unwrap();
    for target in ["open", "claude", "chatgpt"] {
        let (ok, stderr) = login(target, "", &failure);
        assert!(
            !ok && stderr.contains("--key-command is for providers that take an API key"),
            "{stderr}"
        );
        assert!(!stderr.contains("vault is locked"));
    }
    let (ok, stderr) = login("chat:gw", "http://127.0.0.1:2/v1", &failure);
    assert!(
        !ok && stderr.contains("exited with status 2") && !stderr.contains("vault is locked"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&auth).unwrap(), saved_auth);
    assert_eq!(std::fs::read(&providers).unwrap(), saved_providers);

    let (ok, stderr) = login("gw", "", &["cat", key_file]);
    assert!(ok, "{stderr}");
    assert!(!std::fs::read_to_string(&auth).unwrap().contains(secret));
    std::fs::remove_file(&secret_path).unwrap();
    let (ok, stderr) = bart(&["models", "gw"]);
    assert!(
        !ok && stderr.contains("key command `cat` exited with status 1"),
        "{stderr}"
    );
    assert!(!stderr.contains(key_file), "{stderr}");
}
