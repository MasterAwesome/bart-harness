use std::process::Command;

#[test]
#[ignore = "needs tmux"]
fn owned_conversation_preserves_shell_and_draft_through_streaming_resize() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/owned_terminal.py"
        ))
        .arg(env!("CARGO_BIN_EXE_bart"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "needs tmux"]
fn signals_and_job_control_leave_the_terminal_and_children_clean() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/terminal_signals.py"
        ))
        .arg(env!("CARGO_BIN_EXE_bart"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn confirm_and_approval_require_the_current_painted_frame() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/painted_review.py"
        ))
        .arg(env!("CARGO_BIN_EXE_bart"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
