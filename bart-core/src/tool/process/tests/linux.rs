use std::{fs, path::Path};

use super::*;
use crate::confinement::{Confinement, Descendants, Escape, Network, launch};

#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn the_jail_can_read_but_not_write_git_metadata() {
    for (metadata, content) in [
        (".git/config", "[core]\n"),
        (".git", "gitdir: ../main/.git\n"),
        ("inner/config", "[core]\n"),
    ] {
        let root = TempDir::new("jail-git");
        let project = root.path().join("project");
        fs::create_dir_all(project.join(metadata).parent().unwrap()).unwrap();
        fs::write(project.join(metadata), content).unwrap();
        fs::create_dir_all(root.path().join("main/.git")).unwrap();
        if metadata.starts_with("inner") {
            fs::write(project.join(".git"), "gitdir: inner\n").unwrap();
        }
        let context = context(
            &project,
            Confinement::Jailed(Network::Off, Escape::Withheld),
            1000,
        );
        let script = format!("cat {metadata} && echo planted >> {metadata}; echo edited > file");
        let collector = crate::tool::output::Collector::new(
            1000,
            &context.artifacts,
            crate::tool::output::Name::Process {
                cell: context.artifacts.next_cell().unwrap(),
                call: 1,
            },
        )
        .unwrap();
        let live = context.live.start("jail", &script, None);
        let output = Process::spawn(
            &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), script.clone()])
                .unwrap(),
            &context,
            Launch::Session,
        )
        .await
        .unwrap()
        .finish_result(
            collector,
            live.progress(),
            std::time::Duration::from_secs(10),
        )
        .await;
        assert_eq!(output.code, 0, "{output:?}");
        assert!(
            fs::read_to_string(&output.stdout_file.path)
                .unwrap()
                .contains(content.trim_end())
        );
        assert_eq!(
            fs::read_to_string(project.join(metadata)).unwrap(),
            content,
            "{metadata}"
        );
        assert_eq!(
            fs::read_to_string(project.join("file")).unwrap(),
            "edited\n"
        );
    }
}

async fn retention_child(root: &Path) {
    use tokio::io::AsyncReadExt as _;

    let project = root.join("project");
    let context = context(
        &project,
        Confinement::Jailed(Network::On, Escape::Withheld),
        1000,
    );

    let collector = crate::tool::output::Collector::new(
        1000,
        &context.artifacts,
        crate::tool::output::Name::Process {
            cell: context.artifacts.next_cell().unwrap(),
            call: 1,
        },
    )
    .unwrap();
    let script = "sleep 30 >/dev/null 2>&1 & echo $!; exit 0";
    let live = context.live.start("sh", script, None);
    let output = Process::spawn(
        &crate::tool::execution::Argv::parse(vec!["sh".into(), "-c".into(), script.into()])
            .unwrap(),
        &context,
        Launch::Session,
    )
    .await
    .unwrap()
    .finish_result(
        collector,
        live.progress(),
        std::time::Duration::from_secs(10),
    )
    .await;
    assert_eq!(output.code, 0, "{output:?}");
    let retained_pid = fs::read_to_string(&output.stdout_file.path)
        .unwrap()
        .trim()
        .to_owned();
    let retained_count = context.jobs.running_count();
    context.jobs.stop_all();
    context.jobs.settled().await;
    wait_until_dead(&retained_pid).await;
    assert_eq!(
        retained_count, 1,
        "jailed non-bash process with retain_descendants=true must adopt surviving descendants"
    );

    let place = context.place();
    let mut non_retaining = launch(
        "bash",
        &place,
        &Default::default(),
        &place.cwd,
        Descendants::Reap,
    )
    .unwrap();
    non_retaining
        .args(["-c", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut owner = crate::child::Owner::spawn(&mut non_retaining)
        .await
        .unwrap();
    let (_, stdout, _) = owner.pipes();
    let mut stdout = stdout.unwrap();
    let exit = owner.target_exit().await.unwrap();
    let mut printed = String::new();
    let read_pid = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stdout.read_to_string(&mut printed),
    )
    .await;
    let bash_pid = printed.trim().to_owned();
    if exit.survivors {
        owner.kill();
    }
    owner.empty().await.unwrap();
    read_pid
        .expect("timed out reading jailed bash stdout")
        .expect("failed reading jailed bash stdout");
    assert!(
        bash_pid.parse::<u32>().is_ok_and(|pid| pid > 0),
        "expected valid descendant PID from jailed bash, got {bash_pid:?}"
    );
    wait_until_dead(&bash_pid).await;
    assert!(
        !exit.survivors,
        "jailed bash with Descendants::Reap must not retain descendants"
    );
}

#[tokio::test]
async fn jailed_descendant_retention_follows_policy_not_executable_name() {
    use std::os::unix::fs::PermissionsExt as _;

    const CHILD: &str = "BART_JAIL_RETENTION_TEST";
    if let Some(root) = std::env::var_os(CHILD) {
        retention_child(Path::new(&root)).await;
        return;
    }

    let dir = TempDir::new("jail-retention");
    let (project, shims) = (dir.path().join("project"), dir.path().join("shims"));
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&shims).unwrap();

    for (name, script) in [
        ("git", "printf ''"),
        ("python3", "exec /usr/bin/env python3 \"$@\""),
        (
            "systemd-run",
            r#"while [ "$1" != -- ]; do shift; done; shift; exec "$@""#,
        ),
        (
            "bwrap",
            r#"unset BART_SUPERVISOR_SOCKET BART_SUPERVISOR_ADOPT
sup=""
sockdir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --ro-bind)
            if [ "$3" = /run/bart-supervisor ]; then sup="$2"; fi
            if [ "$3" = /run/bart-control ]; then sockdir="$2"; fi
            shift 3
            ;;
        --setenv)
            if [ "$2" = BART_SUPERVISOR_SOCKET ]; then
                export BART_SUPERVISOR_SOCKET="$sockdir/control"
            elif [ "$2" = BART_SUPERVISOR_ADOPT ]; then
                export BART_SUPERVISOR_ADOPT="$3"
            fi
            shift 3
            ;;
        /run/bart-supervisor)
            shift
            exec "$sup" "$@"
            ;;
        *)
            shift
            ;;
    esac
done
exit 1"#,
        ),
    ] {
        let path = shims.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path = format!("{}:/usr/bin:/bin", shims.display());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tool::process::tests::linux::jailed_descendant_retention_follows_policy_not_executable_name",
            "--nocapture",
        ])
        .current_dir(&project)
        .env(CHILD, dir.path())
        .env("PATH", &path)
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
