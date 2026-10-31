use super::*;
use crate::tool::Network;

#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn a_jail_can_read_saved_cells_outside_the_project_but_not_other_files() {
    let dir = TempDir::new("context-archive-jail");
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    let context = Arc::new(
        Context::new(
            project,
            crate::tool::test_support::executable(),
            dir.path().join("saved"),
            1000,
            Confinement::Jailed(Network::On, crate::tool::Escape::Withheld),
            Arc::default(),
        )
        .unwrap(),
    );
    cell(&context, "print('retained evidence')").await;
    let read = cell(
        &context,
        "print(read(cell[1].output) == run([\"bash\", \"-c\", f'cat {cell[1].output}']).stdout)",
    )
    .await;
    assert_eq!(read.text.lines().next(), Some("True"), "{}", read.text);
    assert!(read.text.contains("\"process\":1"));
    let shell = cell(&context, "r=run([\"bash\", \"-c\", 'X=kept; echo \"${X}\"']); assert r.stdout_file.incomplete is None; assert read(r.stdout_file.path) == 'kept\\n'; print(r)").await;
    assert!(
        !shell.is_error() && shell.text.contains("\nkept\n"),
        "{}",
        shell.text
    );
    let outside = dir.path().join("unrelated");
    fs::write(&outside, "must not read").unwrap();
    let hidden = cell(
        &context,
        &format!("read({:?})", outside.display().to_string()),
    )
    .await;
    assert!(hidden.is_error(), "{}", hidden.text);
}

#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn a_jail_without_network_reaches_no_host_service_and_no_web_search() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let probe = format!(
        "import socket\n\
         def reaches():\n    try:\n        socket.create_connection(('127.0.0.1', {port}), 2).close()\n        return True\n    except OSError:\n        return False\n\
         def searches():\n    try:\n        return callable(web_search)\n    except NameError:\n        return 'absent'\n\
         child = run([\"python3\", \"-c\", \"import socket; socket.create_connection((\\\"127.0.0.1\\\", {port}), 2)\"]).return_code == 0\n\
         print(reaches(), child, searches() == 'absent')"
    );
    for (network, expected) in [
        (Network::On, "True True False"),
        (Network::Off, "False False True"),
    ] {
        let dir = TempDir::new("python-jail-network");
        let context = session(
            dir.path(),
            Confinement::Jailed(network, crate::tool::Escape::Withheld),
            10_000,
        );
        context.route_web(Some(crate::tool::WebRoute {
            client: reqwest::Client::new(),
            store: crate::auth::Store::new(dir.path().join("auth.json")),
            provider: crate::provider::Provider::named("chatgpt", dir.path()).unwrap(),
            model: "m".into(),
            effort: None,
            diagnostics: None,
        }));
        let result = cell(&context, &probe).await;
        assert_eq!(
            result.text.lines().next(),
            Some(expected),
            "{network:?}: {}",
            result.text
        );
    }
}

#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn an_interpreter_offering_escape_has_sandbox_on_run_and_bg_run() {
    let dir = TempDir::new("python-jail-escape");
    let context = session(
        dir.path(),
        Confinement::Jailed(Network::On, crate::tool::Escape::Offered),
        10_000,
    );
    text(
        &context,
        "import inspect\n['sandbox' in inspect.signature(f).parameters for f in (run, bg_run)]",
        "[True, True]",
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn jailed_commands_adopt_and_python_interrupts_in_its_own_namespace() {
    let dir = TempDir::new("python-jail-lifetime");
    let context = session(
        dir.path(),
        Confinement::Jailed(Network::On, crate::tool::Escape::Withheld),
        10000,
    );
    let adopted = cell(
        &context,
        "r = run([\"bash\", \"-c\", 'sleep 30 >/dev/null 2>&1 & exit 7'])\nassert r.return_code == 7, r.return_code\nprint(r)",
    )
    .await;
    assert!(
        !adopted.is_error() && adopted.text.contains("\"retained_job\":"),
        "{}",
        adopted.text
    );
    let id = context.live.jobs()[0].job.unwrap();
    let killed = cell(&context, &format!("assert job[{id}].kill() is None")).await;
    assert!(
        !killed.is_error() && killed.text.contains("cancellation requested"),
        "{}",
        killed.text
    );
    assert_eq!(context.jobs.running_count(), 0);
    let exit = cell(
        &context,
        "saved=42; r=run([\"bash\", \"-c\", 'exit 3']); assert r.return_code==3",
    )
    .await;
    assert!(!exit.is_error(), "{}", exit.text);
    assert!(exit.text.contains("\"return_code\":3"));
    let interrupted = call(&json!({"code": "while True: pass", "timeout": 1}), &context).await;
    assert!(
        interrupted.is_error() && interrupted.text.contains("KeyboardInterrupt"),
        "{}",
        interrupted.text
    );
    text(&context, "saved", "42").await;
}
