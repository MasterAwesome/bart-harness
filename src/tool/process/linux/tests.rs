use std::fs;

use super::*;
use crate::tool::test_support::TempDir;

async fn launch_child(root: &Path) {
    let project = root.join("project");
    let context = crate::tool::test_support::context(
        &project,
        crate::tool::Confinement::Jailed(Network::On, crate::tool::Escape::Withheld),
        1000,
    );
    let accepted = tools(&project).unwrap();
    let check = [
        &accepted.python3,
        &accepted.systemd_run,
        &accepted.bwrap,
        &accepted.git,
    ]
    .map(|path| format!("test -x '{}'", path.display()))
    .join(" && ");
    let output = command("bash", &context.place(), &Default::default())
        .unwrap()
        .args(["-c", &check])
        .output()
        .await
        .unwrap();
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(output.status.success(), "{output:?}");
    let output = command("python3", &context.place(), &Default::default())
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("selected {}", accepted.python3.display())),
        "{output:?}"
    );
}

#[tokio::test]
async fn jail_launch_uses_path_tools_outside_the_project() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    const CHILD: &str = "BART_JAIL_TOOLS_TEST";
    if let Some(root) = std::env::var_os(CHILD) {
        launch_child(Path::new(&root)).await;
        return;
    }
    let dir = TempDir::new("jail-tools");
    let (project, outside) = (dir.path().join("project"), dir.path().join("outside"));
    fs::create_dir_all(project.join("bin")).unwrap();
    fs::create_dir(&outside).unwrap();
    let file_links = dir.path().join("file-links");
    fs::create_dir(&file_links).unwrap();
    symlink(project.join("bin"), dir.path().join("link")).unwrap();
    for name in ["python3", "systemd-run", "bwrap", "git"] {
        for (directory, script) in [
            (project.join("bin"), "printf hijacked; exit 77"),
            (outside.clone(), "printf '%s\\n' \"$0\" \"$@\"; env"),
        ] {
            let script = if directory == outside {
                match name {
                    "git" => "printf outside-git",
                    "systemd-run" => {
                        r#"printf '%s\n' "$0" "$@"; env; while [ "$1" != -- ]; do shift; done; shift; exec "$@""#
                    }
                    "bwrap" => {
                        r#"printf '%s\n' "$0" "$@"; while [ "$1" != bash ] && [ "$1" != "$(dirname "$0")/python3" ]; do shift; done; exec "$@""#
                    }
                    "python3" => "printf 'selected %s\n' \"$0\"",
                    _ => script,
                }
            } else {
                script
            };
            let path = directory.join(name);
            fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    for name in ["python3", "systemd-run", "bwrap", "git"] {
        symlink(project.join("bin").join(name), file_links.join(name)).unwrap();
    }
    let path = format!(
        "bin:{}:{}:{}:{}",
        project.join("bin").display(),
        dir.path().join("link").display(),
        file_links.display(),
        outside.display()
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tool::process::linux::tests::jail_launch_uses_path_tools_outside_the_project",
            "--nocapture",
        ])
        .current_dir(&project)
        .env(CHILD, dir.path())
        .env("PATH", &path)
        .env("HOME", dir.path())
        .env("BART_HOST_SECRET", "not-for-children")
        .env("LD_LIBRARY_PATH", &outside)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    let outside = outside.canonicalize().unwrap();
    let outside = outside.display();
    for expected in [
        format!("{outside}/systemd-run"),
        format!("{outside}/bwrap"),
        "outside-git".into(),
    ] {
        assert!(stdout.contains(&expected), "{stdout}");
    }
    assert!(!stdout.contains("hijacked"));
    assert!(!stdout.contains("BART_HOST_SECRET") && !stdout.contains("LD_LIBRARY_PATH"));
    fs::remove_file(dir.path().join("outside/git")).unwrap();
    let error = resolve(path.as_ref(), &project).err().unwrap();
    assert!(error.contains("git"), "{error}");
}

#[test]
fn jail_rejects_ancestors_of_protected_credentials_before_launch() {
    let home = std::env::home_dir().unwrap();
    assert!(crate::tool::process::check_jail(&home).is_err());
    for (path, _) in protected_paths().unwrap() {
        if let Some(parent) = path.ancestors().find(|path| path.is_dir()) {
            assert!(
                crate::tool::process::check_jail(parent).is_err(),
                "{}",
                parent.display()
            );
        }
    }
}

#[test]
fn toolchain_homes_follow_their_variables_and_ignore_empty_ones() {
    let home = Path::new("/home/someone");
    let homes = |cargo: Option<&str>| {
        super::toolchain_homes_from(home, |name| match name {
            "CARGO_HOME" => cargo.map(OsString::from),
            _ => None,
        })
    };
    let defaults = [home.join(".cargo"), home.join(".rustup")];
    assert_eq!(homes(None), defaults);
    assert_eq!(homes(Some("")), defaults, "an empty variable is unset");
    assert_eq!(
        homes(Some("/opt/cargo"))[0],
        Path::new("/opt/cargo"),
        "a set variable is used"
    );
}

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
        let context = crate::tool::test_support::context(
            &project,
            crate::tool::Confinement::Jailed(Network::Off, crate::tool::Escape::Withheld),
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
            &[],
        )
        .unwrap();
        let live = context.live.start("jail", &script, None);
        let output = super::super::Process::spawn(
            &crate::tool::execution::Argv::parse(vec!["bash".into(), "-c".into(), script.clone()])
                .unwrap(),
            &context,
            super::super::Launch::Session,
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

#[test]
fn git_metadata_covers_the_common_directory_and_refuses_what_it_cannot_protect() {
    let root = TempDir::new("git-layouts");
    let project = root.path().join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(project.join("common")).unwrap();
    fs::write(project.join(".git/commondir"), "../common\n").unwrap();
    let found = super::git_metadata(&project).unwrap();
    assert!(
        found.contains(&project.join("common").canonicalize().unwrap()),
        "{found:?}"
    );

    fs::remove_dir_all(project.join(".git")).unwrap();
    let fifo = std::process::Command::new("mkfifo")
        .arg(project.join(".git"))
        .status()
        .unwrap();
    assert!(fifo.success());
    assert_eq!(
        super::git_metadata(&project).unwrap(),
        [project.join(".git")]
    );

    fs::remove_file(project.join(".git")).unwrap();
    fs::write(project.join(".git"), "gitdir: worktree\n").unwrap();
    assert!(super::git_metadata(&project).is_err());

    fs::remove_file(project.join(".git")).unwrap();
    std::os::unix::fs::symlink("common", project.join(".git")).unwrap();
    assert!(super::git_metadata(&project).is_err());
}
