use std::fs;

use super::*;

#[cfg(not(target_os = "linux"))]
mod unsupported;
use crate::tool::test_support::{TempDir, context, wait_until_dead};

#[tokio::test]
async fn cell_process_cannot_write_to_terminal() {
    const CHILD: &str = "BART_TERMINAL_PROBE";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                r#"
import errno, os, pty, select, signal, sys, time
pid, fd = pty.fork()
if pid == 0:
    os.environ['BART_TERMINAL_PROBE'] = '1'
    os.execv(sys.argv[1], [sys.argv[1], '--exact',
        'tool::process::tests::cell_process_cannot_write_to_terminal', '--nocapture'])
data = b''
deadline = time.monotonic() + 30
try:
    while True:
        assert time.monotonic() < deadline, 'terminal probe timed out'
        if not select.select([fd], [], [], 0.1)[0]:
            continue
        try:
            part = os.read(fd, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not part:
            break
        data += part
finally:
    try:
        os.killpg(pid, signal.SIGKILL)
    except PermissionError:
        if os.waitid(os.P_PID, pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
            raise
    os.close(fd)
_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 0, data
assert b'BART_TTY_LEAK' not in data, data
"#,
            ])
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .is_ok()
    );
    let dir = TempDir::new("cell-terminal");
    let context = std::sync::Arc::new(context(dir.path(), Confinement::Unrestricted, 1000));
    let live = context.live.start("python", "terminal probe", None);
    let output = crate::tool::python::call(
        &serde_json::json!({"code": "import subprocess\nassert subprocess.run(['sh', '-c', 'printf BART_TTY_LEAK > /dev/tty'], capture_output=True).returncode != 0\nr = run(['sh', '-c', 'printf BART_TTY_LEAK > /dev/tty'])\nassert r.return_code != 0"}),
        "terminal-probe", &context, &live, None,
    ).await;
    assert!(!output.is_error(), "{}", output.text);
}

#[tokio::test]
async fn typed_results_bound_output_timeout_and_cancellation() {
    let dir = TempDir::new("process-result");
    let context = context(dir.path(), Confinement::Unrestricted, 1000);
    for (command, timeout, success) in [
        (
            "python3 -c 'print(\"x\" * 100000)'",
            Duration::from_secs(5),
            true,
        ),
        ("echo partial; sleep 30", Duration::from_millis(80), false),
    ] {
        let collector = Collector::new(
            1000,
            &context.artifacts,
            crate::tool::output::Name::Process {
                cell: context.artifacts.next_cell().unwrap(),
                call: 1,
            },
        )
        .unwrap();
        let live = context.live.start("test", command, None);
        let result = Process::spawn(
            &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.into()])
                .unwrap(),
            &context,
            super::Launch::Session,
        )
        .await
        .unwrap()
        .finish_result(collector, live.progress(), timeout)
        .await;
        assert_eq!(result.code == 0, success, "{result:?}");
        let wire = serde_json::to_value(&result).unwrap();
        assert!(wire.to_string().len() < 3000);
        if !success {
            assert!(
                fs::read(&result.stdout_file.path)
                    .unwrap()
                    .starts_with(b"partial\n")
            );
        } else {
            assert!(result.stdout_file.incomplete.is_none());
            assert_eq!(
                fs::read(&result.stdout_file.path).unwrap(),
                format!("{}\n", "x".repeat(100000)).as_bytes()
            );
        }
    }
    let collector = Collector::new(
        1000,
        &context.artifacts,
        crate::tool::output::Name::Process {
            cell: context.artifacts.next_cell().unwrap(),
            call: 1,
        },
    )
    .unwrap();
    let live = context.live.start("test", "", None);
    let command = "sleep 30 & echo $! > pid; wait";
    let process = Process::spawn(
        &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.into()])
            .unwrap(),
        &context,
        super::Launch::Session,
    )
    .await
    .unwrap();
    assert!(
        time::timeout(
            Duration::from_millis(200),
            process.finish_result(collector, live.progress(), Duration::from_secs(30))
        )
        .await
        .is_err()
    );
    wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
    #[cfg(target_os = "linux")]
    assert!(
        fs::read_to_string("/proc/thread-self/children")
            .unwrap()
            .trim()
            .is_empty(),
        "dropped command left an unreaped supervisor"
    );
}

#[tokio::test]
async fn adoption_keeps_descendants_and_the_same_output_file_until_killed() {
    let setsid = if cfg!(target_os = "linux") {
        "os.setsid(); "
    } else {
        ""
    };
    let detached =
        format!("import os,time; {setsid}open('pid','w').write(str(os.getpid())); time.sleep(30)");
    let forked = format!(
        "import os,time; p=os.fork(); os._exit(0) if p else None; {setsid}p=os.fork(); os._exit(0) if p else None; open('pid','w').write(str(os.getpid())); time.sleep(30)"
    );
    for (daemon, code, redirect) in [(&detached, 0, ">/dev/null 2>&1"), (&forked, 7, "")] {
        let dir = TempDir::new("adoption");
        let context = context(dir.path(), Confinement::Unrestricted, 10000);
        let command = format!(
            "python3 -c \"{daemon}\" {redirect} & until [ -s pid ]; do sleep 0.01; done; echo before; exit {code}"
        );
        let collector = Collector::new(
            10000,
            &context.artifacts,
            crate::tool::output::Name::Process {
                cell: context.artifacts.next_cell().unwrap(),
                call: 1,
            },
        )
        .unwrap();
        let path = collector.path().to_owned();
        let live = context.live.start("test", &command, None);
        let result = time::timeout(
            Duration::from_secs(5),
            Process::spawn(
                &crate::tool::execution::Argv::parse(vec![
                    "bash".into(),
                    "-c".into(),
                    command.clone(),
                ])
                .unwrap(),
                &context,
                super::Launch::Session,
            )
            .await
            .unwrap()
            .finish_result(collector, live.progress(), Duration::from_secs(10)),
        )
        .await
        .unwrap();
        assert_eq!(result.code, code);
        assert!(result.adopted.is_some(), "{result:?}");
        let jobs = context.live.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(context.jobs.running_count(), 1);
        let id = jobs[0].job.unwrap();
        context.jobs.kill(id).await.unwrap();
        wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("before"), "{saved}");
        assert!(saved.contains("child process output follows"), "{saved}");
    }
}

#[tokio::test]
async fn session_end_kills_adopted_jobs() {
    let dir = TempDir::new("session-adoption");
    let context = context(dir.path(), Confinement::Unrestricted, 10000);
    let command = "sleep 30 & echo $! > pid";
    let collector = Collector::new(
        10000,
        &context.artifacts,
        crate::tool::output::Name::Process {
            cell: context.artifacts.next_cell().unwrap(),
            call: 1,
        },
    )
    .unwrap();
    let live = context.live.start("test", command, None);
    let result = Process::spawn(
        &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.into()])
            .unwrap(),
        &context,
        super::Launch::Session,
    )
    .await
    .unwrap()
    .finish_result(collector, live.progress(), Duration::from_secs(10))
    .await;
    assert_eq!(result.code, 0);
    assert!(result.adopted.is_some());
    context.stop();
    time::timeout(Duration::from_secs(7), context.jobs.settled())
        .await
        .unwrap();
    wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn timeout_and_cancellation_kill_detached_and_double_forked_descendants() {
    for cancel in [false, true] {
        let dir = TempDir::new("deadline-tree");
        let context = context(dir.path(), Confinement::Unrestricted, 10000);
        let command = r#"python3 -c "import os,time; p=os.fork(); os._exit(0) if p else None; os.setsid(); p=os.fork(); os._exit(0) if p else None; open('pid','w').write(str(os.getpid())); time.sleep(30)"; while :; do sleep 1; done"#;
        let collector = Collector::new(
            10000,
            &context.artifacts,
            crate::tool::output::Name::Process {
                cell: context.artifacts.next_cell().unwrap(),
                call: 1,
            },
        )
        .unwrap();
        let live = context.live.start("test", command, None);
        let process = Process::spawn(
            &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.into()])
                .unwrap(),
            &context,
            super::Launch::Session,
        )
        .await
        .unwrap();
        if cancel {
            assert!(
                time::timeout(
                    Duration::from_millis(500),
                    process.finish_result(collector, live.progress(), Duration::from_secs(20))
                )
                .await
                .is_err()
            );
        } else {
            let result = process
                .finish_result(collector, live.progress(), Duration::from_millis(500))
                .await;
            assert!(result.timed_out, "{result:?}");
            assert_ne!(result.code, 0);
            assert!(result.adopted.is_none());
        }
        wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
    }
}

#[tokio::test]
async fn fast_shell_exit_preserves_output_status_and_reaps_the_supervisor() {
    let dir = TempDir::new("fast-shell");
    let context = context(dir.path(), Confinement::Unrestricted, 10000);
    for n in 0..64 {
        let command = format!("printf 'output-{n}'; exit {}", n % 8);
        let collector = Collector::new(
            10000,
            &context.artifacts,
            crate::tool::output::Name::Process {
                cell: context.artifacts.next_cell().unwrap(),
                call: 1,
            },
        )
        .unwrap();
        let live = context.live.start("fast", &command, None);
        let result = Process::spawn(
            &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.clone()])
                .unwrap(),
            &context,
            super::Launch::Session,
        )
        .await
        .unwrap()
        .finish_result(collector, live.progress(), Duration::from_secs(5))
        .await;
        assert_eq!(result.code, n % 8, "{result:?}");
        assert_eq!(
            fs::read_to_string(&result.stdout_file.path).unwrap().trim(),
            format!("output-{n}")
        );
    }
}

#[tokio::test]
async fn pending_exit_never_adopts_after_deadline_or_cancellation() {
    for cancel in [false, true] {
        let dir = TempDir::new("adoption-race");
        let context = context(dir.path(), Confinement::Unrestricted, 10000);
        let command = "sleep 30 & echo $! > pid; exit 0";
        let collector = Collector::new(
            10000,
            &context.artifacts,
            crate::tool::output::Name::Process {
                cell: context.artifacts.next_cell().unwrap(),
                call: 1,
            },
        )
        .unwrap();
        let live = context.live.start("race", command, None);
        let process = Process::spawn(
            &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.into()])
                .unwrap(),
            &context,
            super::Launch::Session,
        )
        .await
        .unwrap();
        while !dir.path().join("pid").exists() {
            time::sleep(Duration::from_millis(5)).await;
        }
        if cancel {
            context.stop();
        }
        let result = process
            .finish_result(
                collector,
                live.progress(),
                if cancel {
                    Duration::from_secs(10)
                } else {
                    Duration::ZERO
                },
            )
            .await;
        assert_ne!(result.code, 0, "{result:?}");
        assert!(result.adopted.is_none(), "{result:?}");
        assert_eq!(context.jobs.running_count(), 0);
        wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
    }
}

#[tokio::test]
async fn adopted_partial_output_continues_once_in_the_same_file_and_job_output() {
    let dir = TempDir::new("adoption-output");
    let context = context(dir.path(), Confinement::Unrestricted, 10000);
    let command = "(while [ ! -e continue ]; do sleep .01; done; printf 'after\\n'; while [ ! -e release ]; do sleep .01; done) & printf before; exit 4";
    let collector = Collector::new(
        10000,
        &context.artifacts,
        crate::tool::output::Name::Process {
            cell: context.artifacts.next_cell().unwrap(),
            call: 1,
        },
    )
    .unwrap();
    let path = collector.path().to_owned();
    let live = context.live.start("test", command, None);
    let result = Process::spawn(
        &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), command.into()])
            .unwrap(),
        &context,
        super::Launch::Session,
    )
    .await
    .unwrap()
    .finish_result(collector, live.progress(), Duration::from_secs(10))
    .await;
    assert_eq!(result.code, 4, "{result:?}");
    assert!(result.stdout_file.incomplete.is_some());
    assert!(
        fs::read(&result.stdout_file.path)
            .unwrap()
            .starts_with(b"before")
    );
    assert!(result.adopted.is_some(), "{result:?}");
    fs::write(dir.path().join("continue"), "").unwrap();
    time::timeout(Duration::from_secs(5), async {
        while !fs::read_to_string(&result.stdout_file.path)
            .unwrap()
            .contains("beforeafter")
        {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fs::write(dir.path().join("release"), "").unwrap();
    context.jobs.settled().await;
    let saved = fs::read_to_string(path).unwrap();
    assert_eq!(saved.matches("beforeafter").count(), 1, "{saved}");
    assert_eq!(
        fs::read(&result.stdout_file.path).unwrap(),
        b"beforeafter\n"
    );
    assert!(fs::read(&result.stderr_file.path).unwrap().is_empty());
    let notes = context.jobs.take_notifications();
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].text.contains("exit 4") && notes[0].text.contains("beforeafter"),
        "{}",
        notes[0].text
    );
}
