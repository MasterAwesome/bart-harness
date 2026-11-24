use super::*;
use crate::tui::tasks::{Kind, TaskResult, Tasks, Ticket};
use std::process::Command as FixtureCommand;
use tokio::net::{TcpListener, TcpStream};

fn shell(args: &[&str]) -> FixtureCommand {
    let mut command = FixtureCommand::new("/bin/sh");
    command.args(args);
    command
}

#[test]
fn selects_clipboard_backends_without_changing_the_environment() {
    use Backend::{Pbcopy, Wayland, Xclip};

    let select = |macos, vars: &[&str]| backends(macos, |name| vars.contains(&name));
    assert_eq!(select(true, &["WAYLAND_DISPLAY", "DISPLAY"]), [Pbcopy]);
    assert_eq!(select(false, &["WAYLAND_DISPLAY"]), [Wayland]);
    assert_eq!(
        select(false, &["WAYLAND_DISPLAY", "DISPLAY"]),
        [Wayland, Xclip]
    );
    assert_eq!(select(false, &["DISPLAY"]), [Xclip]);
    assert_eq!(select(false, &["DISPLAY", "TMUX"]), [Xclip]);
    assert_eq!(select(false, &["TMUX"]), []);
    for ssh in ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"] {
        for macos in [true, false] {
            assert_eq!(
                select(macos, &[ssh, "WAYLAND_DISPLAY", "DISPLAY", "TMUX"]),
                []
            );
        }
    }
}

#[test]
fn terminal_copy_sends_a_plain_osc_52_with_a_utf8_safe_tail() {
    let expected = "x".repeat(MAX_COPY_BYTES - 2);
    let text = format!("prefix💡{expected}");
    let mut output = Vec::new();
    assert_eq!(
        copy_terminal(&mut output, &text, "the answer").unwrap(),
        "sent the last 75 KB of the answer to the terminal clipboard"
    );
    assert_eq!(
        output,
        format!("\x1b]52;c;{}\x07", STANDARD.encode(expected)).as_bytes()
    );
    output.clear();
    assert_eq!(
        copy_terminal(&mut output, "abc", "the answer").unwrap(),
        "sent the answer to the terminal clipboard"
    );
    assert_eq!(output, b"\x1b]52;c;YWJj\x07");
}

#[tokio::test]
async fn native_copy_waits_for_eof_and_leaves_the_clipboard_holder_running() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let script = r#"
        cat > "$1/input" || exit 1
        (
            for step in 1 2 3 4 5 6 7 8 9 10; do
                if [ -f "$1/paste" ]; then
                    cat "$1/input" > "$1/output"
                    exit
                fi
                sleep 0.2
            done
        ) &
    "#;
    let args = ["-c", script, "fixture", path];
    let text = "  **text** 💡\n".repeat(8192);
    assert!(try_native(&[Backend::Wayland], |_| shell(&args), &text, TIMEOUT).await);
    std::fs::write(dir.path().join("paste"), []).unwrap();
    let output = dir.path().join("output");
    time::timeout(TIMEOUT, async {
        loop {
            if std::fs::read_to_string(&output)
                .as_deref()
                .is_ok_and(|read| *read == text)
            {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn missing_tool_falls_through_to_next_helper() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let output = dir.path().join("clipboard");
    let working = ["-c", "cat > \"$1\"", "fixture", output.to_str().unwrap()];
    let command = |tool| match tool {
        Backend::Wayland => FixtureCommand::new(&missing),
        Backend::Xclip => shell(&working),
        Backend::Pbcopy => unreachable!(),
    };
    assert!(!try_native(&[Backend::Wayland], command, "", TIMEOUT).await);
    assert!(try_native(&[Backend::Wayland, Backend::Xclip], command, "", TIMEOUT).await);
    assert_eq!(std::fs::read(output).unwrap(), [] as [u8; 0]);
}

#[tokio::test]
async fn native_backends_offer_utf8_plain_text() {
    let wayland = Backend::Wayland.command();
    assert_eq!(wayland.get_program(), "wl-copy");
    assert_eq!(
        wayland.get_args().collect::<Vec<_>>(),
        ["--type", "text/plain;charset=utf-8"]
    );
    assert!(Backend::Wayland.accepts("%PDF-1.4"));
    let mac = Backend::Pbcopy.command();
    assert_eq!(mac.get_program(), "pbcopy");
    assert!(
        mac.get_envs()
            .any(|(key, value)| key == "LC_CTYPE" && value.is_some_and(|value| value == "UTF-8"))
    );
    assert!(
        mac.get_envs()
            .any(|(key, value)| key == "LC_ALL" && value.is_none())
    );
    assert!(Backend::Pbcopy.accepts("💡"));
    for text in [r"{\rtf1 fixture}", "%!PS-Adobe-3.0 EPSF-3.0"] {
        assert!(
            !try_native(
                &[Backend::Pbcopy],
                |_| panic!("pbcopy must not run"),
                text,
                TIMEOUT
            )
            .await
        );
    }
}

#[tokio::test]
async fn early_stdin_close_falls_through_even_on_successful_exit() {
    let early = || shell(&["-c", "exec 0<&-; exit 0"]);
    let text = "copy".repeat(1024 * 1024);
    let error = time::timeout(TIMEOUT, write_native(early(), &text))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("clipboard");
    let working = ["-c", "cat > \"$1\"", "fixture", output.to_str().unwrap()];
    assert!(
        try_native(
            &[Backend::Wayland, Backend::Xclip],
            |tool| match tool {
                Backend::Wayland => early(),
                _ => shell(&working),
            },
            &text,
            TIMEOUT
        )
        .await
    );
    assert_eq!(std::fs::read_to_string(output).unwrap(), text);
}

struct ProcessPair(TcpListener);

impl ProcessPair {
    async fn new() -> Self {
        Self(TcpListener::bind("127.0.0.1:0").await.unwrap())
    }

    fn command(&self) -> FixtureCommand {
        let program = r"
import os, socket, sys
sys.stdin.buffer.read()
leader = os.fork() != 0
with socket.create_connection(('127.0.0.1', int(sys.argv[1]))) as conn:
    conn.sendall(b'L' if leader else b'D')
    conn.recv(1)
os._exit(1)
";
        let mut command = FixtureCommand::new("python3");
        command.args([
            "-c",
            program,
            &self.0.local_addr().unwrap().port().to_string(),
        ]);
        command
    }

    async fn ready(&self) -> (TcpStream, TcpStream) {
        time::timeout(TIMEOUT, async {
            let (mut first, _) = self.0.accept().await.unwrap();
            let (mut second, _) = self.0.accept().await.unwrap();
            match (
                first.read_u8().await.unwrap(),
                second.read_u8().await.unwrap(),
            ) {
                (b'L', b'D') => (first, second),
                (b'D', b'L') => (second, first),
                _ => panic!("expected one leader and one descendant"),
            }
        })
        .await
        .unwrap()
    }
}

async fn assert_stopped((mut leader, mut descendant): (TcpStream, TcpStream)) {
    time::timeout(TIMEOUT, async {
        let (mut leader_byte, mut descendant_byte) = ([0], [0]);
        let (leader, descendant) = tokio::join!(
            leader.read(&mut leader_byte),
            descendant.read(&mut descendant_byte),
        );
        assert_eq!(leader.unwrap(), 0);
        assert_eq!(descendant.unwrap(), 0);
    })
    .await
    .expect("the process group must close both connections");
}

#[tokio::test]
async fn timeout_stops_the_ready_leader_and_descendant() {
    let fixture = ProcessPair::new().await;
    let mut copy = Box::pin(write_native(fixture.command(), "copy"));
    let pair = tokio::select! {
        pair = fixture.ready() => pair,
        result = &mut copy => panic!("helper exited early: {result:?}"),
    };
    assert!(
        time::timeout(Duration::from_millis(25), copy)
            .await
            .is_err()
    );
    assert_stopped(pair).await;
}

#[tokio::test]
async fn nonzero_exit_stops_the_ready_descendant() {
    let fixture = ProcessPair::new().await;
    let mut copy = Box::pin(write_native(fixture.command(), "copy"));
    let mut pair = tokio::select! {
        pair = fixture.ready() => pair,
        result = &mut copy => panic!("helper exited early: {result:?}"),
    };
    pair.0.write_all(b"x").await.unwrap();
    assert_eq!(
        time::timeout(TIMEOUT, copy).await.unwrap().unwrap().code(),
        Some(1)
    );
    assert_stopped(pair).await;
}

#[tokio::test]
async fn newer_copy_stops_the_ready_leader_and_descendant() {
    let fixture = ProcessPair::new().await;
    let command = fixture.command();
    let mut tasks = Tasks::default();
    let ticket = |seq| Ticket {
        kind: Kind::Copy,
        seq,
    };
    tasks.spawn_copy(ticket(0), async move {
        let result = write_native(command, "old").await;
        panic!("superseded copy returned: {result:?}");
    });
    let pair = fixture.ready().await;
    tasks.spawn_copy(ticket(1), async {
        let command = shell(&["-c", "cat >/dev/null"]);
        assert!(write_native(command, "new").await.unwrap().success());
        TaskResult::Copied("new answer".into())
    });
    let done = time::timeout(TIMEOUT, tasks.next()).await.unwrap().unwrap();
    assert_eq!(done.ticket, ticket(1));
    assert!(matches!(done.result, TaskResult::Copied(what) if what == "new answer"));
    assert_stopped(pair).await;
}

#[tokio::test]
async fn timeout_covers_a_helper_that_never_reads_stdin() {
    let args = ["-c", "exec sleep 30"];
    assert!(
        !try_native(
            &[Backend::Pbcopy],
            |_| shell(&args),
            &"text".repeat(1024 * 1024),
            Duration::from_millis(100),
        )
        .await
    );
}
