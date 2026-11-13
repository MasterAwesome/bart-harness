#[test]
fn supervisor_mode_is_hidden() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bart"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("bart-process-supervisor"));
}

#[test]
fn bart_death_or_group_termination_cleans_descendants_without_zombie_leaks_or_foreign_kills() {
    for mode in ["kill", "group-term"] {
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                include_str!("fixtures/process-lifetime.py"),
                env!("CARGO_BIN_EXE_bart"),
                mode,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn malformed_control_requests_report_failure_after_cleaning_the_scope() {
    let output = std::process::Command::new("python3").args(["-c", r#"
import errno, json, os, pathlib, socket, subprocess, sys, tempfile, time
for request in [b'{"Unknown":{}}\n', b'x' * 4097]:
    with tempfile.TemporaryDirectory(prefix='bart-protocol-') as directory:
        root = pathlib.Path(directory)
        listener = socket.socket(socket.AF_UNIX); listener.bind(str(root/'control')); listener.listen(1); listener.settimeout(5)
        child = subprocess.Popen([sys.argv[1], '--bart-process-supervisor', 'bash', '-c', 'sleep 30 & echo $! > pid; wait'], cwd=root,
            env=dict(os.environ, BART_SUPERVISOR_SOCKET=str(root/'control'), BART_SUPERVISOR_ADOPT='1'), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        stream, _ = listener.accept(); stream.settimeout(7); reader = stream.makefile('rb')
        try:
            stream.sendall(b'{"Start":{"exit_on_exec":false}}\n')
            assert json.loads(reader.readline()) == {'Started': {}}
            deadline = time.monotonic() + 5
            while not (root/'pid').exists() or not (root/'pid').read_text().strip():
                assert time.monotonic() < deadline, 'target did not start'; time.sleep(.01)
            pid = int((root/'pid').read_text())
            stream.sendall(request)
            event = json.loads(reader.readline())
            assert 'Failed' in event, event
            assert child.wait(timeout=7) == 1
            state = subprocess.run(['ps','-o','stat=','-p',str(pid)],capture_output=True,text=True).stdout.strip()[:1]
            assert state in ['','Z'], f'protocol error left descendant {pid} alive'
        finally:
            try: stream.shutdown(socket.SHUT_WR)
            except OSError as error:
                if error.errno != errno.ENOTCONN: raise
            reader.close(); stream.close(); listener.close()
            child.wait(timeout=7)
"#, env!("CARGO_BIN_EXE_bart")]).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn keeping_jobs_survives_the_actual_cli_exit_but_not_cancellation() {
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/keep_background.py"
        ))
        .arg(env!("CARGO_BIN_EXE_bart"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
