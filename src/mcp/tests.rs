use std::time::Duration;

use serde_json::json;
use std::path::Path;

use crate::tool::test_support::{TempDir, wait_until_dead};

use super::*;

const FIXTURE: &str = include_str!("fixtures/server.py");

fn registry(dir: &Path, env: &Value) -> Arc<Registry> {
    let config = Config::parse(
            &json!({ "mcpServers": { "fx": { "command": "python3", "args": ["-c", FIXTURE], "env": env } } })
                .to_string())
        .unwrap();
    Registry::start(
        &config,
        crate::tool::test_support::place(dir, crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.join("auth.json")),
    )
}

#[tokio::test]
async fn changes_follow_installed_states_and_stop() {
    let dir = TempDir::new("mcp-changes");
    let mcp = registry(dir.path(), &json!({}));
    let mut changes = mcp.changes();
    tokio::time::timeout(Duration::from_secs(10), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(mcp.states()[0].1, State::Ready { .. }));

    let disabled = config::Entry::new(json!({"command": "python3", "disabled": true}));
    mcp.apply("fx", &disabled);
    changes.changed().await.unwrap();
    assert!(matches!(mcp.states()[0].1, State::Disabled(_)));

    let failing = config::Entry::new(json!({"command": "/nonexistent/bart-mcp-test"}));
    mcp.apply("fx", &failing);
    changes.changed().await.unwrap();
    assert!(matches!(mcp.states()[0].1, State::Starting));
    tokio::time::timeout(Duration::from_secs(10), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(mcp.states()[0].1, State::Failed(_)));

    mcp.stop();
    changes.changed().await.unwrap();
    assert!(mcp.is_empty());
}

#[tokio::test]
async fn a_stdio_server_is_listed_described_called_and_stopped() {
    let dir = TempDir::new("mcp-stdio");
    for env in [json!({}), json!({ "FIXTURE_MODERN": "1" })] {
        let mcp = registry(dir.path(), &env);
        assert!(
            mcp.tools(None, Some("zebra"))
                .await
                .unwrap()
                .contains("echo"),
            "input-property descriptions are searched"
        );
        assert!(
            mcp.tools(None, Some("kiwi"))
                .await
                .unwrap()
                .contains("echo"),
            "nested input properties are searched too"
        );
        assert!(
            mcp.tools(None, Some("ECHO"))
                .await
                .unwrap()
                .contains("echo")
        );
        assert!(mcp.tools(Some("nope"), None).await.is_err());
        assert!(
            mcp.tools(None, Some("grandchild starts"))
                .await
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .contains("spawn"),
            "words match apart, in any order"
        );
        let described = mcp.describe("fx", "echo").await.unwrap();
        assert!(
            described.contains("More detail.") && described.contains("\"type\": \"object\""),
            "{described}"
        );
        let arguments = json!({ "a": [1, 2] }).as_object().unwrap().clone();
        let echoed = mcp.call("fx", "echo", arguments).await.unwrap();
        assert_eq!(
            (echoed.value, echoed.notes),
            (json!({ "a": [1, 2] }), Vec::<String>::new())
        );
        assert!(described.contains("server-provided guidance:\nA test server.\nSecond line."));
        assert!(
            mcp.call("fx", "fail", Map::new())
                .await
                .unwrap()
                .error
                .is_some()
        );
        assert!(mcp.call("fx", "absent", Map::new()).await.is_err());
        let grandchild = mcp.call("fx", "spawn", Map::new()).await.unwrap().value;
        drop(mcp);
        wait_until_dead(grandchild.as_str().unwrap()).await;
    }
}

#[tokio::test]
async fn errors_never_repeat_configured_secrets_or_reach_the_terminal_raw() {
    let dir = TempDir::new("mcp-secrets");
    let store = Store::new(dir.path().join("auth.json"));
    store
        .update(async |credentials| {
            let token = json!({
                "access_token": "synthetic-oauth-access-token",
                "refresh_token": "abc123",
                "token_type": "Bearer",
            });
            credentials
                .mcp
                .insert("docs".into(), crate::auth::SecretValue(token));
            Ok(())
        })
        .await
        .unwrap();
    let entry = json!({
        "url": "https://example.invalid/mcp",
        "headers": { "Authorization": "Bearer synthetic-token" }
    });
    let echoed = format!(
        "denied: synthetic-token\u{1b}[31m synthetic-oauth-access-token abc123 Bearer\n{}",
        "x\n".repeat(1 << 20)
    );
    let shown = clean(
        &echoed,
        &secrets(
            "docs",
            Some(&config::resolve(&config::Entry::new(entry)).unwrap()),
            &store,
        ),
    );
    assert!(
        !shown.contains("synthetic") && !shown.contains('\u{1b}'),
        "{shown}"
    );
    assert!(
        shown.contains("denied") && !shown.contains("abc123"),
        "{shown}"
    );
    let huge = clean(&format!("cause: {}", "é".repeat(10_000)), &[]);
    assert!(huge.len() <= 4096 && huge.starts_with("cause:"));
    let multiline = clean(
        "multiline-secret\nsecond-half",
        &["multiline-secret\nsecond-half".to_owned()],
    );
    assert!(!multiline.contains("multiline-secret") && !multiline.contains("second-half"));
    assert!(
        shown.lines().count() <= 64,
        "a 2 MiB body is not printed whole"
    );
}

#[tokio::test]
async fn a_server_that_fails_to_start_says_why() {
    let dir = TempDir::new("mcp-fail");
    let mcp = registry(dir.path(), &json!({ "FIXTURE_FAIL": "1" }));
    let found = mcp.tools(Some("fx"), None).await.unwrap_err();
    assert!(found.contains("FIXTURE_FAIL is set"), "{found}");
    assert_eq!(mcp.servers()[0]["status"], "failed");
}

#[tokio::test]
async fn results_keep_extra_content_and_images_even_with_errors() {
    let dir = TempDir::new("mcp-mixed");
    let mcp = registry(dir.path(), &json!({}));
    let mixed = mcp.call("fx", "mixed", Map::new()).await.unwrap();
    assert_eq!(mixed.value, json!({"rows": 2}));
    assert_eq!(
        mixed.notes[0], "note: rows are capped",
        "the duplicate JSON text is dropped"
    );
    for (note, names) in mixed.notes[1..].iter().zip([
        ["file:///report.csv", "resource_link"],
        ["audio/wav", "audio"],
        ["file:///raw", "application/octet-stream"],
    ]) {
        assert!(names.iter().all(|name| note.contains(name)), "{note}");
    }
    let odd = mixed
        .notes
        .iter()
        .find(|note| note.contains("odd"))
        .unwrap();
    assert!(
        !odd.contains('\n') && odd.len() < 400,
        "metadata stays one bounded line"
    );
    assert_eq!(mixed.images.len(), 1);
    let broken = mcp.call("fx", "broken", Map::new()).await.unwrap();
    assert!(broken.error.unwrap().contains("render failed"));
    assert_eq!(broken.images.len(), 1, "an error keeps its image");
}

#[tokio::test]
async fn status_is_immediate_and_discovery_shares_a_deadline_without_stopping_queued_startup() {
    let dir = TempDir::new("mcp-deadline");
    let gate = dir.path().join("release");
    let mut entries = serde_json::Map::new();
    entries.insert(
        "a-ready".into(),
        json!({"command":"python3", "args":["-c", FIXTURE]}),
    );
    for name in ["b", "c", "d", "e", "z-queued"] {
        entries.insert(
            name.into(),
            json!({"command":"python3", "args":["-c", FIXTURE], "env": {
                "FIXTURE_START_GATE": gate, "FIXTURE_PID_FILE": dir.path().join(name)
            }}),
        );
    }
    let config = Config::parse(&json!({"mcpServers": entries}).to_string()).unwrap();
    let mcp = Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    assert!(
        mcp.servers()
            .as_array()
            .unwrap()
            .iter()
            .all(|server| server["status"] == "starting")
    );
    mcp.describe("a-ready", "echo").await.unwrap();
    time::timeout(Duration::from_secs(10), async {
        while !["b", "c", "d", "e"]
            .iter()
            .all(|name| dir.path().join(name).exists())
        {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !dir.path().join("z-queued").exists(),
        "only four workers start at once"
    );
    time::pause();
    let first = tokio::spawn({
        let mcp = Arc::clone(&mcp);
        async move { mcp.tools(None, Some("echo")).await.unwrap() }
    });
    let second = tokio::spawn({
        let mcp = Arc::clone(&mcp);
        async move { mcp.tools(None, None).await.unwrap() }
    });
    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(61)).await;
    time::resume();
    for found in [
        first.await.unwrap(),
        second.await.unwrap(),
        mcp.tools(None, None).await.unwrap(),
    ] {
        assert!(
            found.contains("tool \"echo\"")
                && found.contains("z-queued")
                && found.contains("not searched"),
            "{found}"
        );
    }
    std::fs::write(&gate, "release").unwrap();
    assert!(
        mcp.call("z-queued", "echo", Map::new()).await.is_ok(),
        "queued startup continues after discovery expires"
    );
    assert!(dir.path().join("z-queued").exists());
    mcp.apply(
        "z-queued",
        &config::Entry::new(
            json!({"command":"python3", "args":["-c", FIXTURE], "env":{"FIXTURE_START_HANG":"1"}}),
        ),
    );
    assert!(
        time::timeout(Duration::from_secs(1), mcp.tools(None, None))
            .await
            .unwrap()
            .unwrap()
            .contains("still starting"),
        "restarting a server does not reset discovery's deadline"
    );
}

#[tokio::test]
async fn a_server_that_exits_becomes_failed_but_a_tool_error_does_not() {
    let dir = TempDir::new("mcp-exit");
    let pid_file = dir.path().join("pid");
    let mcp = registry(dir.path(), &json!({"FIXTURE_PID_FILE":pid_file}));
    assert!(
        mcp.call("fx", "fail", Map::new())
            .await
            .unwrap()
            .error
            .is_some()
    );
    assert_eq!(mcp.servers()[0]["status"], "ready");
    assert!(
        time::timeout(
            Duration::from_millis(50),
            mcp.call("fx", "hang", Map::new())
        )
        .await
        .is_err()
    );
    assert!(
        mcp.call("fx", "echo", Map::new()).await.is_ok(),
        "request interruption leaves the connection usable"
    );
    let mut status = mcp.statuses().remove(0).1;
    let pid = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    time::timeout(
        Duration::from_secs(10),
        status.wait_for(|state| matches!(state, Status::Failed(_))),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        mcp.call("fx", "echo", Map::new()).await.is_err(),
        "failure is sticky"
    );
}

#[tokio::test]
async fn a_tool_past_the_listing_limit_is_found_by_its_input_description() {
    let dir = TempDir::new("mcp-many");
    let mcp = registry(
        dir.path(),
        &json!({ "FIXTURE_MANY_TOOLS": "1", "FIXTURE_GUIDANCE": "界".repeat(2000) }),
    );
    let all = mcp.tools(Some("fx"), None).await.unwrap();
    assert!(all.contains("(500 more; narrow with `query`)") && !all.contains("tool650"));
    let found = mcp.tools(None, Some("quokka")).await.unwrap();
    assert_eq!(found, "server \"fx\", tool \"tool650\" — A numbered tool.");
    let description = mcp.describe("fx", "tool650").await.unwrap();
    let guidance = description
        .split_once("server-provided guidance:\n")
        .unwrap()
        .1;
    let (kept, _) = guidance.split_once("\n[guidance truncated]").unwrap();
    assert!(kept.len() <= 4096 && kept.chars().all(|c| c == '界'));
}

#[tokio::test]
async fn toggling_a_server_stops_or_starts_it_and_persists() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new("mcp-toggle");
    let mut config = Config::default();
    config
        .add(
            "fx",
            config::Entry::new(json!({ "command": "python3", "args": ["-c", FIXTURE] })),
        )
        .unwrap();
    config.save(&path(dir.path())).unwrap();
    let mcp = Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    let grandchild = mcp.call("fx", "spawn", Map::new()).await.unwrap().value;
    let catalog = mcp.catalog("fx").unwrap();
    assert!(mcp.call("fx", "echo", Map::new()).await.is_ok());
    assert!(catalog.tools().any(|(name, _)| name == "echo"));

    std::fs::write(path(dir.path()).with_extension("lock"), "").unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(toggle("fx", dir.path(), &mcp).await.is_err());
    assert_eq!(mcp.names(), ["fx"]);
    assert!(
        !Config::load(&path(dir.path()))
            .unwrap()
            .entries()
            .next()
            .unwrap()
            .1
            .disabled()
    );
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o300)).unwrap();
    let (enabled, warning) = toggle("fx", dir.path(), &mcp).await.unwrap();
    assert!(!enabled);
    assert!(warning.is_some(), "directory could not be opened for sync");
    wait_until_dead(grandchild.as_str().unwrap()).await;
    assert!(mcp.names().is_empty(), "new sessions leave it out");
    assert!(mcp.catalog("fx").is_none());
    assert!(catalog.tools().any(|(name, _)| name == "echo"));
    assert!(mcp.call("fx", "echo", Map::new()).await.is_err());
    let saved = Config::load(&path(dir.path())).unwrap();
    assert!(saved.entries().next().unwrap().1.disabled());

    let (enabled, warning) = toggle("fx", dir.path(), &mcp).await.unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(enabled);
    assert!(warning.is_some());
    assert!(mcp.call("fx", "echo", Map::new()).await.is_ok());
    assert!(
        !Config::load(&path(dir.path()))
            .unwrap()
            .entries()
            .next()
            .unwrap()
            .1
            .disabled()
    );
    assert!(toggle("absent", dir.path(), &mcp).await.is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn jailed_stdio_servers_run_confined_with_their_environment() {
    use crate::tool::{Confinement, Escape, Network};
    let dir = TempDir::new("mcp-jail");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let sentinel = "sentinel-7f3a91c2";
    let home = tempfile::NamedTempFile::new_in(std::env::home_dir().unwrap()).unwrap();
    let config = Config::parse(
        &json!({ "mcpServers": {
            "fx": { "command": "python3", "args": ["-c", FIXTURE], "env": {
                "FIXTURE_SECRET": sentinel, "FIXTURE_HOME": home.path(), "FIXTURE_EMPTY": "", "FIXTURE_BIG": "b".repeat(8192),
                "FIXTURE_SLEEP": "301.25", "FIXTURE_NETWORK_PORT": port } },
            "remote": { "url": "http://127.0.0.1:9/mcp" },
            "reserved": { "command": "python3", "args": ["-c", FIXTURE], "env": { "BART_SUPERVISOR_ADOPT": "1" } },
        } })
        .to_string())
    .unwrap();
    for network in [Network::On, Network::Off] {
        let mcp = Registry::start(
            &config,
            crate::tool::test_support::place(
                &project,
                Confinement::Jailed(network, Escape::Withheld),
            ),
            &Client::new(),
            &Store::new(dir.path().join("auth.json")),
        );
        let env = mcp.call("fx", "env", Map::new()).await.unwrap().value;
        assert_eq!(
            (
                env["secret"].as_str(),
                env["empty"].as_str(),
                env["big"].as_u64()
            ),
            (Some(sentinel), Some(""), Some(8192))
        );
        assert_eq!(env["network"], network == Network::On);
        assert_eq!(
            env["home"],
            json!([true, false]),
            "home is visible read-only"
        );
        assert_eq!(
            env["env_fds"],
            json!([]),
            "bubblewrap closes its argument descriptor"
        );
        let leaked = std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
            std::fs::read(entry.path().join("cmdline")).is_ok_and(|line| {
                line.windows(sentinel.len())
                    .any(|w| w == sentinel.as_bytes())
            })
        });
        assert!(!leaked, "the environment never appears on a command line");
        let remote = mcp
            .servers()
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == "remote")
            .unwrap()
            .clone();
        assert_eq!(
            remote["status"] == "disabled",
            network == Network::Off,
            "{remote}"
        );
        let reserved = mcp.tools(Some("reserved"), None).await.unwrap_err();
        assert!(reserved.contains("reserved"), "{reserved}");
        mcp.call("fx", "spawn", Map::new()).await.unwrap();
        let context = crate::tool::Context::new(
            project.clone(),
            crate::tool::test_support::executable(),
            project.join("spill"),
            1000,
            Confinement::Jailed(network, Escape::Withheld),
            Arc::clone(&mcp),
        )
        .unwrap();
        let replacement = Registry::start(
            &config,
            crate::tool::test_support::place(&project, Confinement::Unrestricted),
            &Client::new(),
            &Store::new(dir.path().join("auth.json")),
        );
        context
            .reconfine(Confinement::Unrestricted, Arc::clone(&replacement))
            .await
            .1
            .unwrap();
        assert!(
            mcp.is_empty(),
            "reconfinement stops the old registry even with retained callers"
        );
        let host = replacement
            .call("fx", "env", Map::new())
            .await
            .unwrap()
            .value;
        assert_eq!(
            host["home"],
            json!([true, true]),
            "the replacement uses the new confinement"
        );
        context
            .reconfine(
                Confinement::Jailed(network, Escape::Withheld),
                Arc::default(),
            )
            .await
            .1
            .unwrap();
        assert!(
            replacement.is_empty(),
            "host servers do not survive the switch back into jail"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
                std::fs::read(entry.path().join("cmdline"))
                    .is_ok_and(|line| line.windows(6).any(|w| w == b"301.25"))
            }) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the server's descendants are reaped");
    }
}

#[tokio::test]
async fn network_off_disables_remote_servers_before_resolution_or_connection() {
    use crate::tool::{Confinement, Escape, Network};
    let dir = TempDir::new("mcp-remote-policy");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config::parse(
        &json!({"mcpServers": {
            "remote": {"url": format!("http://{}/mcp", listener.local_addr().unwrap()),
                "headers":{"Authorization":"Bearer ${BART_MCP_UNSET_POLICY_TEST}"}},
            "user-off": {"command":"python3", "disabled":true}
        }})
        .to_string(),
    )
    .unwrap();
    let mcp = Registry::start(
        &config,
        crate::tool::test_support::place(
            dir.path(),
            Confinement::Jailed(Network::Off, Escape::Withheld),
        ),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    assert!(mcp.names().is_empty());
    let records = mcp.servers();
    assert_eq!(records[0]["status"], "disabled");
    assert!(
        records[0]["reason"]
            .as_str()
            .unwrap()
            .contains("--no-network")
    );
    assert_eq!(records[1]["status"], "disabled");
    assert!(
        mcp.call("remote", "echo", Map::new())
            .await
            .err()
            .unwrap()
            .contains("--no-network")
    );
    assert!(
        time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn replacing_a_starting_server_cleans_it_up_and_ignores_its_late_completion() {
    let dir = TempDir::new("mcp-replacement");
    let pid_file = dir.path().join("pid");
    let gate = dir.path().join("gate");
    let mcp = registry(
        dir.path(),
        &json!({"FIXTURE_START_GATE":gate,"FIXTURE_PID_FILE":pid_file}),
    );
    time::timeout(Duration::from_secs(10), async {
        while !pid_file.exists() {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    mcp.apply(
        "fx",
        &config::Entry::new(json!({"command":"python3", "args":["-c",FIXTURE]})),
    );
    assert!(mcp.call("fx", "echo", Map::new()).await.is_ok());
    std::fs::write(&gate, "release").unwrap();
    wait_until_dead(&pid).await;
    assert_eq!(mcp.servers()[0]["status"], "ready");
    mcp.apply(
        "fx",
        &config::Entry::new(json!({"command":"python3", "disabled":true})),
    );
    assert_eq!(mcp.servers()[0]["status"], "disabled");
}

#[tokio::test]
async fn a_secret_cut_by_the_stderr_tail_is_not_reported() {
    let dir = TempDir::new("mcp-tail-secret");
    let line: String = (0..5 * 1024)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let multiline = format!("{}\n{}", &line[..3000], &line[3000..]);
    for (token, split) in [(&line, false), (&line, true), (&multiline, false)] {
        let mut env = json!({ "FIXTURE_FAIL": "1", "API_TOKEN": token, "A_TOKEN": &line[..40] });
        if split {
            env["FIXTURE_SPLIT"] = json!("1");
        }
        let mcp = registry(dir.path(), &env);
        let error = mcp.tools(Some("fx"), None).await.unwrap_err();
        assert!(error.contains("FIXTURE_FAIL is set"), "{error}");
        for piece in token
            .as_bytes()
            .chunks(16)
            .map(|piece| std::str::from_utf8(piece).unwrap())
        {
            assert!(!error.contains(piece), "{piece} leaked: {error}");
        }
    }
}

#[tokio::test]
async fn discovery_reports_a_server_that_failed_while_it_waited() {
    let dir = TempDir::new("mcp-stale");
    let config = Config::parse(
        &json!({ "mcpServers": {
            "fx": { "command": "python3", "args": ["-c", FIXTURE], "env": { "FIXTURE_PID_FILE": dir.path().join("pid") } },
            "slow": { "command": "python3", "args": ["-c", FIXTURE], "env": { "FIXTURE_START_HANG": "1" } },
        } })
        .to_string())
    .unwrap();
    let mut mcp = Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    Arc::get_mut(&mut mcp).unwrap().discovery = Some(time::Instant::now() + Duration::from_secs(3));
    mcp.tools(Some("fx"), None).await.unwrap();
    let pid = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    let (found, ()) = tokio::join!(mcp.tools(None, Some("echo")), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();
    });
    let found = found.unwrap();
    assert!(
        found.contains("unavailable") && found.contains("fx") && !found.contains("tool \"echo\""),
        "{found}"
    );
}
