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
