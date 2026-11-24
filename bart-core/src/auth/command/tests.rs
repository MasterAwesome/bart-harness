use std::{fs, time::Instant};

use super::*;
use crate::tool::test_support::{TempDir, wait_until_dead};

fn sh(script: &str, dir: &Path) -> Vec<String> {
    ["sh", "-c", script, "sh", dir.to_str().unwrap()]
        .map(str::to_owned)
        .to_vec()
}

const ENDPOINT: &str = "https://fixture.invalid/v1";

async fn issue(store: &Store, argv: &[String]) -> Result<Issued, IssueError> {
    resolve(store, "test", ENDPOINT, argv, |_| true).await
}

fn runs(dir: &Path) -> usize {
    fs::read_to_string(dir.join("runs"))
        .unwrap()
        .lines()
        .count()
}

async fn save(store: &Store, credential: Credential) {
    store
        .update(async |credentials| {
            credentials.providers.insert("test".into(), credential);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn shared_cache_rejects_only_the_issued_generation_even_for_identical_keys() {
    let dir = TempDir::new("key-command-cache");
    let store = Store::new(dir.path().join("auth.json"));
    let clone = store.clone();
    let argv = sh(
        r#"echo run >> "$1/runs"; printf '  same-key\n'"#,
        dir.path(),
    );
    let (first, old) = tokio::try_join!(issue(&store, &argv), issue(&clone, &argv)).unwrap();
    assert_eq!(first.key().expose(), "same-key");
    assert_eq!(old.key(), first.key());
    assert_eq!(runs(dir.path()), 1);

    first.reject();
    let fresh = issue(&clone, &argv).await.unwrap();
    assert_eq!(fresh.key(), first.key());
    assert_eq!(runs(dir.path()), 2);
    old.reject();
    assert_eq!(issue(&store, &argv).await.unwrap().key(), fresh.key());
    assert_eq!(runs(dir.path()), 2);
}

#[tokio::test]
async fn concurrent_resolvers_share_one_failure_but_a_later_call_runs_again() {
    let dir = TempDir::new("key-command-shared-failure");
    let store = Store::new(dir.path().join("auth.json"));
    let argv = sh(r#"echo run >> "$1/runs"; exit 3"#, dir.path());
    let outcomes = futures_util::future::join_all((0..6).map(|_| issue(&store, &argv))).await;
    assert_eq!(runs(dir.path()), 1);
    for outcome in outcomes {
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "key command `sh` exited with status 3"
        );
    }
    assert!(issue(&store, &argv).await.is_err());
    assert_eq!(runs(dir.path()), 2);
}

#[tokio::test]
async fn a_stale_flight_never_publishes_or_caches_the_key() {
    let dir = TempDir::new("key-command-stale-flight");
    let store = Store::new(dir.path().join("auth.json"));
    let argv = sh(
        r#"echo run >> "$1/runs"
        touch "$1/started"
        while [ ! -f "$1/release" ]; do sleep 0.01; done
        cat "$1/key""#,
        dir.path(),
    );
    let current = std::cell::Cell::new(true);
    let leader = resolve(&store, "test", ENDPOINT, &argv, |_| current.get());
    let waiter = issue(&store, &argv);
    let change = async {
        let started = dir.path().join("started");
        let key = dir.path().join("key");
        let release = dir.path().join("release");
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        current.set(false);
        fs::write(key, "stale-key").unwrap();
        fs::write(release, "").unwrap();
    };
    let (leader, waiter, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(biased; leader, waiter, change)
    })
    .await
    .unwrap();
    assert!(matches!(leader, Err(IssueError::StaleBinding)));
    assert!(matches!(waiter, Err(IssueError::StaleBinding)));
    assert_eq!(runs(dir.path()), 1);

    fs::write(dir.path().join("key"), "fresh-key").unwrap();
    assert_eq!(
        issue(&store, &argv).await.unwrap().key().expose(),
        "fresh-key"
    );
    let cached = resolve(&store, "test", ENDPOINT, &argv, |_| current.get()).await;
    assert!(matches!(cached, Err(IssueError::StaleBinding)));
    assert_eq!(runs(dir.path()), 2);
}

#[tokio::test]
async fn store_read_forgets_a_replaced_command_and_argv_changes_start_a_new_slot() {
    let dir = TempDir::new("key-command-replaced");
    let path = dir.path().join("auth.json");
    let store = Store::new(path.clone());
    let writer = Store::new(path);
    let argv = sh(r#"echo run >> "$1/runs"; echo key"#, dir.path());
    let command = Credential::Command {
        endpoint: ENDPOINT.into(),
        argv: argv.clone(),
    };
    save(&writer, command.clone()).await;
    store.load().unwrap();
    issue(&store, &argv).await.unwrap();
    save(
        &writer,
        Credential::ApiKey {
            key: super::super::api_key("static-key").unwrap(),
        },
    )
    .await;
    assert!(matches!(
        store.load().unwrap().providers["test"],
        Credential::ApiKey { .. }
    ));
    save(&writer, command).await;
    issue(&store, &argv).await.unwrap();
    assert_eq!(runs(dir.path()), 2);

    let changed = sh("echo different-key", dir.path());
    save(
        &writer,
        Credential::Command {
            endpoint: ENDPOINT.into(),
            argv: changed.clone(),
        },
    )
    .await;
    store.load().unwrap();
    assert_eq!(
        issue(&store, &changed).await.unwrap().key().expose(),
        "different-key"
    );
}

#[tokio::test]
async fn saving_a_different_endpoint_evicts_the_cached_key_without_a_read() {
    let dir = TempDir::new("key-command-saved");
    let store = Store::new(dir.path().join("auth.json"));
    let writer = store.clone();
    let argv = sh(r#"echo run >> "$1/runs"; echo key"#, dir.path());
    let mut credentials = Credentials::default();
    let command = Credential::Command {
        endpoint: ENDPOINT.into(),
        argv: argv.clone(),
    };
    credentials.providers.insert("test".into(), command.clone());
    writer.lock().await.unwrap().save(&credentials).unwrap();
    issue(&store, &argv).await.unwrap();
    assert_eq!(runs(dir.path()), 1);

    credentials.providers.insert(
        "test".into(),
        Credential::Command {
            endpoint: "https://replacement.invalid/v1".into(),
            argv: argv.clone(),
        },
    );
    writer.lock().await.unwrap().save(&credentials).unwrap();
    credentials.providers.insert("test".into(), command);
    writer.lock().await.unwrap().save(&credentials).unwrap();
    issue(&store, &argv).await.unwrap();
    assert_eq!(runs(dir.path()), 2);
}

#[tokio::test]
async fn neither_login_nor_resolution_discloses_child_diagnostics() {
    let dir = TempDir::new("key-command-diagnostics");
    let store = Store::new(dir.path().join("auth.json"));
    let argv = sh(
        r#"if [ -f "$1/ready" ]; then echo key; exit; fi
        echo stdout-secret
        printf '%05000d\n' 0 >&2
        printf '\033[31mvault is locked; stdout-secret; other-secret\n' >&2
        exit 3"#,
        dir.path(),
    );
    let login = verify(&argv).await.unwrap_err().to_string();
    assert_eq!(login, "key command `sh` exited with status 3");
    let runtime = issue(&store, &argv).await.unwrap_err().to_string();
    assert_eq!(runtime, "key command `sh` exited with status 3");
    fs::write(dir.path().join("ready"), "").unwrap();
    assert_eq!(issue(&store, &argv).await.unwrap().key().expose(), "key");
}

#[tokio::test]
async fn login_rejects_invalid_commands_and_reports_why_a_helper_produced_no_key() {
    let dir = TempDir::new("key-command-invalid");
    for argv in [
        vec![],
        vec!["./key".into()],
        vec!["sh".into(), "a\0b".into()],
    ] {
        assert!(matches!(verify(&argv).await, Err(Error::KeyCommand(_))));
    }
    let output = verify(&sh("echo empty-vault >&2", dir.path()))
        .await
        .unwrap_err()
        .to_string();
    assert!(output.ends_with("printed no key"), "{output}");
    let output = verify(&sh("echo two words", dir.path()))
        .await
        .unwrap_err()
        .to_string();
    assert!(output.contains("did not print one token"), "{output}");
}

#[tokio::test]
async fn runtime_reports_only_known_exec_diagnostics() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("key-command-exec");
    let store = Store::new(dir.path().join("auth.json"));
    let path = dir.path().join("key-command");
    let argv = [path.to_str().unwrap().to_owned()];
    let missing = issue(&store, &argv).await.unwrap_err().to_string();
    assert!(missing.ends_with("cannot run: not found"), "{missing}");
    fs::write(&path, "#!/bin/sh\necho key").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let denied = issue(&store, &argv).await.unwrap_err().to_string();
    assert!(
        denied.ends_with("cannot run: permission denied"),
        "{denied}"
    );

    let spoofed = sh(
        "echo 'bart: cannot execute detached process: private detail' >&2; exit 127",
        dir.path(),
    );
    let err = issue(&store, &spoofed).await.unwrap_err().to_string();
    assert_eq!(err, "key command `sh` exited with status 127");
}

#[tokio::test]
async fn the_command_runs_without_a_terminal_or_stdin_from_home_and_keeps_literal_arguments() {
    let dir = TempDir::new("key-command-environment");
    let home = std::env::var_os("HOME")
        .filter(|path| Path::new(path).is_absolute())
        .unwrap_or_else(|| "/".into());
    let mut argv = sh(
        r#"[ "$(pwd -P)" = "$(cd "$3" && pwd -P)" ] || exit 10
        read -r line && exit 11
        sh -c ': </dev/tty' 2>/dev/null && exit 12
        [ "$2" = '$HOME; $(false)' ] || exit 13
        echo ok"#,
        dir.path(),
    );
    argv.push("$HOME; $(false)".into());
    argv.push(home.into_string().unwrap());
    verify(&argv).await.unwrap();
}

async fn descendant(dir: &Path) -> String {
    let path = dir.join("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(&path)
            && text.trim().parse::<u32>().is_ok_and(|pid| pid > 0)
        {
            return text.trim().into();
        }
        assert!(
            Instant::now() < deadline,
            "the command did not spawn a descendant"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn timeout_and_oversized_stdout_kill_the_group() {
    for (after_spawn, failure) in [
        ("wait", "timed out after 2s"),
        ("yes", "printed more than 64 KiB"),
    ] {
        let dir = TempDir::new("key-command-kill");
        let argv = sh(
            &format!(r#"sleep 60 & echo $! > "$1/pid"; {after_spawn}"#),
            dir.path(),
        );
        let err = verify(&argv).await.unwrap_err().to_string();
        assert!(err.ends_with(failure), "{err}");
        wait_until_dead(&descendant(dir.path()).await).await;
    }
}

#[tokio::test]
async fn cancelling_a_resolution_kills_the_group_and_allows_another_run() {
    let dir = TempDir::new("key-command-cancel");
    let store = Store::new(dir.path().join("auth.json"));
    let argv = sh(
        r#"if [ -f "$1/pid" ]; then echo key; exit; fi; sleep 60 & echo $! > "$1/pid"; wait"#,
        dir.path(),
    );
    let running = {
        let (store, argv) = (store.clone(), argv.clone());
        tokio::spawn(async move { issue(&store, &argv).await })
    };
    let pid = descendant(dir.path()).await;
    let (issued, ()) = tokio::join!(issue(&store, &argv), async {
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
    },);
    wait_until_dead(&pid).await;
    assert_eq!(issued.unwrap().key().expose(), "key");
}
