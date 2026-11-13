use std::{fs, path::Path, time::Instant};

use super::*;
use crate::tool::{
    Confinement, Status,
    test_support::{TempDir, context, wait_until_dead},
};

#[cfg(target_os = "linux")]
mod linux;

fn session(dir: &Path, confinement: Confinement, budget: usize) -> Arc<Context> {
    Arc::new(context(dir, confinement, budget))
}

async fn call(arguments: &Value, context: &Arc<Context>) -> Output {
    let mut live = context.live.start(
        "python",
        arguments["code"].as_str().unwrap_or_default(),
        None,
    );
    let output = super::call(arguments, "call", context, &live, None).await;
    live.result(&output.text);
    live.finish(output.status);
    output
}

async fn cell(context: &Arc<Context>, code: &str) -> Output {
    call(&json!({ "code": code }), context).await
}

async fn text(context: &Arc<Context>, code: &str, expected: &str) {
    let output = cell(context, code).await;
    assert_eq!(output.status, Status::Succeeded, "{}", output.text);
    assert_eq!(output.text, expected, "{code}");
}

fn python_has_expression_positions() -> bool {
    let output = std::process::Command::new("python3")
        .args(["-c", "import sys; print(sys.version_info >= (3, 11))"])
        .output()
        .expect("python3 must be available");
    assert!(output.status.success());
    output.stdout == b"True\n"
}

async fn refused(context: &Arc<Context>, code: &str) {
    let output = cell(context, code).await;
    assert!(output.is_error(), "{code}: {}", output.text);
}

#[tokio::test]
async fn cells_keep_state_and_show_output_values_and_failures() {
    let dir = TempDir::new("python-cells");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let first = cell(
        &context,
        "x = 41\nprint('hi')\nimport os\nos.system('echo child')",
    )
    .await;
    assert_eq!(first.status, Status::Succeeded, "{}", first.text);
    assert_eq!(
        first.text, "hi\nchild\n0",
        "prints, child output and the final value"
    );
    text(&context, "x + 1", "42").await;
    for code in [
        "print('visible')",
        "print('visible')\nNone",
        "print('visible')\ntime.sleep(0)",
    ] {
        text(&context, code, "visible").await;
    }
    text(&context, "0", "0").await;
    text(&context, "False", "False").await;
    text(&context, "print(None)", "None").await;
    let failed = cell(&context, "y = 1\n1 / 0").await;
    assert_eq!(failed.status, Status::Failed);
    assert!(
        failed.text.contains("].code\", line 2")
            && failed.text.contains("    1 / 0")
            && failed.text.ends_with("ZeroDivisionError: division by zero"),
        "{}",
        failed.text
    );
    text(&context, "x, y", "(41, 1)").await;
    text(
        &context,
        "pathlib.Path('a/b').name, json.dumps(re.split(',', 'p,q'))",
        "('b', '[\"p\", \"q\"]')",
    )
    .await;
    let missing = cell(&context, "textwrap.dedent('')").await;
    assert!(
        missing
            .text
            .trim_end_matches('?')
            .ends_with("forget to import 'textwrap'"),
        "a missing standard module names its import: {}",
        missing.text
    );
    let rebinding = cell(
        &context,
        "globals()['edit'] = 1\nfor read in ['mine']: pass\nread",
    )
    .await;
    assert!(
        rebinding.text.starts_with("'mine'")
            && rebinding.text.ends_with(&format!(
                "[Bart cell: cell[{}] helper bindings removed; edit, read are built-ins]",
                rebinding.cell.unwrap()
            )),
        "a cell's rebinding works inside it and is reported: {}",
        rebinding.text
    );
    text(&context, "callable(read) and callable(edit)", "True").await;
    assert!(
        cell(&context, "run([\"bash\", \"-c\", 'true'], sandbox=False)")
            .await
            .text
            .ends_with("TypeError: run() got an unexpected keyword argument 'sandbox'"),
        "without an offered escape, run has no sandbox parameter"
    );
    assert!(
        cell(&context, "input()").await.text.contains("EOFError"),
        "a cell has no input"
    );
    assert_eq!(cell(&context, "pass").await.status, Status::Succeeded);
}

#[tokio::test]
async fn a_rewind_after_an_interrupted_cell_restarts_python_and_shows_nothing_from_before() {
    let dir = TempDir::new("python-rewind");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    text(&context, "x = 1\nx", "1").await;
    let interrupted = cell(
        &context,
        "print('STALE', flush=True)\nimport time\ntime.sleep(30)",
    );
    assert!(
        time::timeout(Duration::from_secs(1), interrupted)
            .await
            .is_err()
    );
    context.reset_python();
    let after = cell(&context, "'x' in globals()").await;
    assert_eq!(after.status, Status::Succeeded, "{}", after.text);
    assert!(
        after.text[..after.prefix_len.unwrap()].ends_with("False"),
        "{}",
        after.text
    );
    assert!(!after.text.contains("STALE"), "{}", after.text);
}

#[tokio::test]
async fn direct_arguments_and_abnormal_exits_preserve_prior_effects() {
    let dir = TempDir::new("native-run");
    let context = files(dir.path());
    let invalid = cell(
        &context,
        r#"for argv in ('echo', [], [''], ['true', '\0']):
    try:
        run(argv)
    except BartError:
        pass
    else:
        raise AssertionError('invalid argv accepted')
"#,
    )
    .await;
    assert!(!invalid.is_error(), "{}", invalid.text);
    let result = cell(&context, r#"import sys,json
r=run([sys.executable,'-c','import sys,json;print(json.dumps(sys.argv[1:]))','a ; $(touch bad) *','space name'])
assert json.loads(r.stdout)==['a ; $(touch bad) *','space name']
assert not pathlib.Path('bad').exists()
for code in (0,2,17,255):
    r=run([sys.executable,'-c',f'raise SystemExit({code})'], check=False)
    assert r.return_code==code and bool(r)==(code==0)
write('before','kept')
run([sys.executable,'-c','import os,signal;os.kill(os.getpid(),signal.SIGTERM)'])
write('after','not authorized')
"#).await;
    assert!(
        result.is_error() && result.text.contains("RunError"),
        "{}",
        result.text
    );
    assert!(dir.path().join("before").exists() && !dir.path().join("after").exists());
    let recovered = cell(
        &context,
        r#"try:
    run([sys.executable,'-c','import os,signal;os.kill(os.getpid(),signal.SIGTERM)'])
except RunError as e:
    assert e.result.return_code==-15
    print('recovered')
assert not hasattr(__import__('builtins'),'sh')
"#,
    )
    .await;
    assert!(!recovered.is_error(), "{}", recovered.text);
    let abnormal = cell(
        &context,
        r#"try:
    run([sys.executable,'-c','import os,signal;os.kill(os.getpid(),signal.SIGTERM)'])
except RunError as e:
    assert e.result.outcome=={'signal':15} and e.result.return_code==-15
else:
    raise AssertionError('signal was accepted as ordinary exit')
try:
    run(['no-such-bart-program'])
except BartError:
    pass
else:
    raise AssertionError('missing executable returned a Result')
"#,
    )
    .await;
    assert!(!abnormal.is_error(), "{}", abnormal.text);
}

#[tokio::test]
async fn run_checks_nonzero_exits_by_default_and_can_opt_out() {
    let dir = TempDir::new("python-run-check");
    let context = files(dir.path());
    for options in ["", ", check=True"] {
        let output = cell(
            &context,
            &format!("run(['python3', '-c', 'print(123); raise SystemExit(7)']{options})\nwrite('after', 'must not run')"),
        )
        .await;
        assert!(
            output.is_error() && output.text.contains("RunError"),
            "{}",
            output.text
        );
        assert!(!dir.path().join("after").exists());
    }
    let output = cell(
        &context,
        r#"
argv = ['python3', '-c', 'import sys; print("out"); print("err", file=sys.stderr); sys.exit(7)']
try:
    run(argv)
except RunError as error:
    r = error.result
    assert r.return_code == 7 and r.outcome == {'exit': 7}
    assert r.stdout == 'out\n' and r.stderr == 'err\n'
else:
    raise AssertionError('nonzero exit did not raise')
r = run(argv, check=False)
assert r.return_code == 7 and not r
assert r.stdout == 'out\n' and r.stderr == 'err\n'
assert run(['python3', '-c', 'pass'], check=True).return_code == 0
"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
}

#[tokio::test]
async fn exec_failures_report_posix_exit_codes_and_clean_cwd_errors() {
    let dir = TempDir::new("python-exec-posix");
    let context = files(dir.path());
    fs::write(dir.path().join("plain.txt"), "not executable\n").unwrap();
    fs::create_dir(dir.path().join("subdir")).unwrap();
    let script = dir.path().join("bad-shebang.sh");
    fs::write(&script, "#!/no/such/bart/interpreter\necho hi\n").unwrap();
    fs::set_permissions(
        &script,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    let output = cell(
        &context,
        r"
try:
    run(['no-such-bart-program'])
except RunError as error:
    assert error.result.return_code == 127
    assert error.result.outcome == {'exit': 127}
    assert error.result.stderr.strip()
    assert 'supervisor' not in str(error).lower()
    assert 'os error' not in str(error).lower()
else:
    raise AssertionError('missing executable did not raise RunError')

missing = run(['no-such-bart-program'], check=False)
assert missing.return_code == 127
assert missing.stderr.strip()
assert 'supervisor' not in repr(missing).lower()
assert 'os error' not in repr(missing).lower()

bad_interp = run(['./bad-shebang.sh'], check=False)
assert bad_interp.return_code == 127
assert 'interpreter' in bad_interp.stderr.lower()

oversize = run(['/' + 'a' * 5_000], check=False)
assert oversize.return_code == 126
assert 0 < len(oversize.stderr) < 4096
assert '...' in oversize.stderr
assert 'supervisor' not in oversize.stderr.lower()
assert 'os error' not in oversize.stderr.lower()

for unexec in ('./plain.txt', './subdir', './plain.txt/nested'):
    try:
        run([unexec])
    except RunError as error:
        assert error.result.return_code == 126, (unexec, error.result.return_code)
    else:
        raise AssertionError(f'{unexec} did not raise RunError')
    probe = run([unexec], check=False)
    assert probe.return_code == 126, (unexec, probe.return_code)
    assert probe.stderr.strip()
    assert 'supervisor' not in repr(probe).lower()
    assert 'os error' not in repr(probe).lower()

for bad_cwd in ('no-such-cwd', 'plain.txt'):
    try:
        run(['true'], cwd=bad_cwd)
    except BartError as error:
        assert not isinstance(error, RunError)
        assert 'supervisor' not in str(error).lower()
        assert 'os error' not in str(error).lower()
    else:
        raise AssertionError(f'{bad_cwd} was accepted as cwd')

bg_missing = bg_run(['no-such-bart-program'])
",
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
        .await
        .unwrap();
    let settled = cell(
        &context,
        r"
assert bg_missing.return_code == 127
assert bg_missing.stderr.strip()
assert 'supervisor' not in bg_missing.stderr.lower()
assert 'os error' not in bg_missing.stderr.lower()
",
    )
    .await;
    assert!(!settled.is_error(), "{}", settled.text);
}

#[tokio::test]
async fn run_preserves_failure_adopts_survivors_and_kills_on_timeout() {
    let dir = TempDir::new("python-sh");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let result = cell(
        &context,
        "r = run([\"bash\", \"-c\", 'echo out; echo err >&2; exit 3'], check=False)\nr.stdout, r.stderr, r.return_code, bool(r)",
    )
    .await;
    assert!(!result.is_error(), "{}", result.text);
    assert!(
        result.text.contains(", 3, False)")
            && result.text.contains("out\\n")
            && result.text.contains("err\\n"),
        "{}",
        result.text
    );
    let started = Instant::now();
    let left = cell(
        &context,
        "r = run([\"bash\", \"-c\", 'sleep 30 & echo $! > pid']); print(r.return_code)",
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a leftover process held sh open"
    );
    assert!(
        left.text.contains("left processes running as job[") && left.text.contains("exit 0"),
        "{}",
        left.text
    );
    let id = context.live.jobs()[0].job.unwrap();
    let killed = cell(&context, &format!("assert job[{id}].kill() is None")).await;
    assert!(
        !killed.is_error() && killed.text.contains("cancellation requested"),
        "{}",
        killed.text
    );
    wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
    for command in ["sleep 30", "exec >&- 2>&-; sleep 30"] {
        let started = Instant::now();
        let output = cell(
            &context,
            &format!(
                "try:\n    run([\"bash\", \"-c\", {command:?}],timeout=1)\nexcept RunError as e:\n    assert e.result.timed_out\nelse:\n    raise AssertionError('timeout returned')"
            ),
        )
        .await;
        assert!(!output.is_error(), "{}", output.text);
        assert!(output.text.contains("] killed"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    let output = cell(
        &context,
        "r=run([\"bash\", \"-c\", 'exec >&- 2>&-; sleep 1; exit 4'], check=False); assert r.return_code==4",
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
}

#[tokio::test]
async fn timeout_gracefully_interrupts_and_preserves_state() {
    let dir = TempDir::new("python-graceful-interrupt");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    cell(&context, "x = 42").await;
    let started = Instant::now();
    let timed_out = call(
        &json!({ "code": "run([\"bash\", \"-c\", 'echo $$ > command; sleep 30'])", "timeout": 1 }),
        &context,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(timed_out.status, Status::Failed);
    let pid = |name| fs::read_to_string(dir.path().join(name)).unwrap();
    wait_until_dead(pid("command").trim()).await;
    text(&context, "x", "42").await;
}

#[tokio::test]
async fn long_output_is_bounded_and_saved() {
    let dir = TempDir::new("python-shape");
    let context = session(dir.path(), Confinement::Unrestricted, 2000);
    let long = cell(&context, "for n in range(1, 25001): print(n)").await;
    assert!(long.text.len() < 2000 + 400, "{} bytes", long.text.len());
    assert!(
        long.text.contains("Bart output:") && long.text.starts_with("1\n2\n"),
        "{}",
        long.text
    );
    let saved = long.saved.as_ref().unwrap();
    assert!(saved.is_complete());
    let number = long.cell.unwrap();
    assert!(
        long.text.contains(&format!("cell[{number}].remaining()")),
        "{}",
        long.text
    );
    text(
        &context,
        &format!("print(len(read(cell[{number}].output).splitlines()))"),
        "25000",
    )
    .await;
    let path = &saved.path;
    assert_eq!(
        fs::read_to_string(path).unwrap().lines().count(),
        25_000,
        "the saved output is whole"
    );
    let full = fs::read_to_string(path).unwrap();
    assert!(full.len() > 100_000);
    let mut combined = long
        .text
        .strip_suffix(long.notes.trim())
        .unwrap()
        .to_owned();
    let mut pages = 0;
    while combined.len() < full.len() {
        let prefix = if pages == 0 {
            "print('α😀before ' * 30, end='')\n"
        } else {
            ""
        };
        let display = if pages % 2 == 0 {
            "part"
        } else {
            "print(part)"
        };
        let code = format!(
            "{prefix}part = cell[{number}].remaining()\nwith open('part', 'wb') as file: size = file.write(part.encode())\n{display}"
        );
        let output = cell(&context, &code).await;
        assert!(
            !output.is_error() && output.notes.is_empty(),
            "{}",
            output.text
        );
        assert!(output.text.len() <= 2000);
        assert_eq!(output.text.matches("[Bart output:").count(), 1);
        let part = fs::read_to_string(dir.path().join("part")).unwrap();
        assert!(!part.is_empty());
        combined.push_str(&part);
        assert!(full.starts_with(&combined));
        pages += 1;
    }
    assert!(pages > 3);
    assert_eq!(combined, full);
    text(
        &context,
        &format!("cell[{number}].remaining()"),
        &format!(
            "[Bart output: cell[{number}] {} of {} bytes shown]",
            full.len(),
            full.len()
        ),
    )
    .await;
    let unknown = cell(&context, "cell[2].remaining()").await;
    assert!(unknown.is_error() && unknown.text.contains("read(cell[2].output)"));
}

#[tokio::test]
async fn cells_reach_mcp_servers() {
    let dir = TempDir::new("python-mcp");
    let fixture = include_str!("../../../mcp/fixtures/server.py");
    let config = crate::mcp::Config::parse(
        &json!({ "mcpServers": { "com.example / staging'$": { "command": "python3", "args": ["-c", fixture] } } })
            .to_string())
    .unwrap();
    let client = reqwest::Client::new();
    let store = crate::auth::Store::new(dir.path().join("auth.json"));
    let registry = crate::mcp::Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &client,
        &store,
    );
    let context = Arc::new(
        Context::new(
            dir.path().to_path_buf(),
            crate::tool::test_support::executable(),
            dir.path().join("spill"),
            10_000,
            Confinement::Unrestricted,
            registry,
        )
        .unwrap(),
    );
    assert!(
        cell(&context, "mcp.servers()")
            .await
            .text
            .contains("com.example / staging'$")
    );
    assert!(
        cell(&context, "mcp.tools(query='echo')")
            .await
            .text
            .contains("echo")
    );
    text(
        &context,
        "r = mcp(\"com.example / staging'$\", 'echo', a=[1, 2])\nr['a'][1]",
        "2",
    )
    .await;
    text(
        &context,
        "r = mcp(\"com.example / staging'$\", 'echo', server='s', tool='t')\nr['server'] + r['tool']",
        "'st'",
    )
    .await;
    let started = std::time::Instant::now();
    let hung = call(
        &json!({ "code": "mcp(\"com.example / staging'$\", 'hang')", "timeout": 1 }),
        &context,
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout covers helpers"
    );
    assert!(hung.is_error(), "{}", hung.text);
    assert!(!hung.text.contains("is still running"), "{}", hung.text);
    text(&context, "6*7", "42").await;
    let failed = cell(&context, "mcp(\"com.example / staging'$\", 'fail')").await;
    assert!(
        failed.is_error() && failed.text.contains("it failed"),
        "{}",
        failed.text
    );
    let mixed = cell(
        &context,
        "r = mcp(\"com.example / staging'$\", 'mixed')\nr['rows']",
    )
    .await;
    assert!(
        mixed.text.contains("note: rows are capped")
            && mixed.text.contains("file:///report.csv")
            && mixed.text.ends_with('2'),
        "{}",
        mixed.text
    );
    assert_eq!(mixed.images.len(), 1, "the image reaches the model");
    let broken = cell(&context, "mcp(\"com.example / staging'$\", 'broken')").await;
    assert!(
        broken.is_error() && broken.text.contains("render failed"),
        "{}",
        broken.text
    );
    assert_eq!(broken.images.len(), 1);
}

#[tokio::test]
async fn background_output_grows_and_ended_jobs_can_be_killed() {
    let dir = TempDir::new("python-jobs");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let started = cell(&context,
        "j = bg_run([\"bash\", \"-c\", 'echo first; while [ ! -e next ]; do sleep 0.02; done; echo second; while [ ! -e release ]; do sleep 0.02; done'], notify='^first$')"
    ).await;
    time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
        .await
        .unwrap();
    context.jobs.take_notifications();
    let first = cell(&context, "print(j)").await;
    assert!(
        !first.is_error() && first.text.contains("first"),
        "{}",
        first.text
    );
    assert!(!first.text.contains("second"), "{}", first.text);
    fs::write(dir.path().join("next"), "").unwrap();
    let id = context.live.jobs()[0].id;
    wait_live(&context, id, "second\n").await;
    let second = cell(&context, "print(j)").await;
    assert!(
        !second.is_error() && second.text.contains("first\nsecond"),
        "{}",
        second.text
    );
    assert_eq!(context.jobs.running_count(), 1);
    fs::write(dir.path().join("release"), "").unwrap();
    assert_eq!(started.jobs.len(), 1, "{}", started.text);
    context.jobs.wake_ready().await;
    assert_eq!(context.jobs.running_count(), 0);
    assert!(
        context.jobs.any(),
        "undelivered output is not a running job"
    );
    let mut finished = context.jobs.take_notifications();
    let notification = finished.remove(0);
    assert!(
        notification.text.contains("first\nsecond"),
        "{notification:?}"
    );
    let ended = cell(&context, "print(j)\nj.kill()\nj.kill()").await;
    assert!(
        !ended.is_error() && ended.text.contains("first\nsecond"),
        "{}",
        ended.text
    );

    let killed = cell(
        &context,
        "j = bg_run([\"bash\", \"-c\", 'sleep 30'])\nassert j.kill() is None",
    )
    .await;
    assert!(!killed.is_error(), "{}", killed.text);
    assert_eq!(context.jobs.running_count(), 0);
    context.jobs.wake_ready().await;
    let cancelled = context.jobs.take_notifications();
    assert_eq!(cancelled.len(), 1);
    let cancelled = cancelled.into_iter().next().unwrap();
    let crate::history::NotificationKind::JobFinished {
        id,
        outcome,
        command,
        ..
    } = cancelled.kind
    else {
        panic!("{:?}", cancelled.kind)
    };
    assert_eq!(
        (id, outcome, command.as_str()),
        (
            killed.jobs[0],
            crate::history::JobOutcome::Killed,
            r#"["bash","-c","sleep 30"]"#
        )
    );
    assert!(!cell(&context, "j.kill()").await.is_error());
}
async fn wait_live(context: &Arc<Context>, id: u64, word: &str) {
    time::timeout(Duration::from_secs(5), async {
        loop {
            if context
                .live
                .snapshot(id)
                .is_some_and(|snapshot| snapshot.output.ends_with(word))
            {
                return;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn files(dir: &Path) -> Arc<Context> {
    session(dir, Confinement::Unrestricted, 400_000)
}

type WriteCase<'a> = (&'a str, Option<&'a [u8]>, &'a str, &'a [u8]);

type EditCase<'a> = (&'a [u8], &'a str, &'a str, bool, Option<&'a [u8]>);

fn temporaries(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".bart-"))
        .collect()
}

fn py(text: &str) -> String {
    format!("{text:?}")
}

#[tokio::test]
async fn read_returns_complete_raw_text_and_displays_bounded_views() {
    let dir = TempDir::new("helper-read");
    fs::write(dir.path().join("crlf.txt"), "a\r\nb\r\n").unwrap();
    fs::write(dir.path().join("five.txt"), "1\n2\n3\n4\n5").unwrap();
    fs::write(
        dir.path().join("long.txt"),
        format!("{}\nend\n", "x".repeat(22_000)),
    )
    .unwrap();
    fs::write(dir.path().join("empty.txt"), "").unwrap();
    let context = files(dir.path());
    for (code, expected) in [
        ("read('crlf.txt')", vec![(1, "a"), (2, "b")]),
        (
            "read('five.txt', offset=2, limit=2)",
            vec![(2, "2"), (3, "3")],
        ),
        ("read('five.txt', limit=2)", vec![(1, "1"), (2, "2")]),
        ("read('five.txt', offset=5, limit=9)", vec![(5, "5")]),
    ] {
        let output = cell(&context, code).await;
        assert!(!output.is_error(), "{}", output.text);
        let rows: Vec<_> = output
            .text
            .lines()
            .filter_map(|row| {
                let row = row.trim_start();
                let split = row.find(char::is_whitespace)?;
                Some((
                    row[..split].parse::<usize>().ok()?,
                    row[split..].trim_start(),
                ))
            })
            .collect();
        assert_eq!(rows, expected, "{code}");
        assert!(!output.text.contains('\r'));
    }
    text(
        &context,
        r#"data = read('crlf.txt')
assert data == 'a\r\nb\r\n'
assert data.splitlines(keepends=True) == ['a\r\n', 'b\r\n']
import copy, pickle
assert repr(copy.deepcopy(data)) == repr(data)
assert pickle.loads(pickle.dumps(data)) == data
assert json.loads(json.dumps({'data':data})) == {'data':'a\r\nb\r\n'}
assert read('five.txt', offset=2, limit=2) == '2\n3\n'
assert read('five.txt', offset=0) == read('five.txt', offset=1)
assert read('five.txt', offset=9) == read('five.txt', limit=0) == read('empty.txt') == ''
assert 'empty file' not in repr(read('empty.txt', limit=0))
print('stored silently')"#,
        "stored silently",
    )
    .await;
    text(&context, "print(read('five.txt'))", "1\n2\n3\n4\n5").await;
    fs::write(
        dir.path().join("emoji.txt"),
        format!("{}\nend\n", "🙂".repeat(700)),
    )
    .unwrap();
    text(
        &context,
        "data = read('emoji.txt')\nassert data == '🙂' * 700 + '\\nend\\n'\nprint('intact')",
        "intact",
    )
    .await;
    for code in ["read('long.txt')", "read('emoji.txt')"] {
        let cut = cell(&context, code).await;
        assert!(!cut.is_error(), "{}", cut.text);
        assert!(
            cut.text.contains("     1\t") && cut.text.ends_with("     2\tend"),
            "{}",
            cut.text
        );
    }
    let large = "αβγ".repeat(20) + "\n";
    fs::write(dir.path().join("many.txt"), large.repeat(3000)).unwrap();
    let output = cell(
        &context,
        r#"data = read('many.txt')
assert len(data.splitlines()) == 3000
assert data.endswith('αβγ' * 20 + '\n')
view = repr(data)
assert '  3000\t' in view
assert view.endswith('αβγ' * 20)
print('complete data; complete numbered representation')"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert_eq!(
        output.text.trim(),
        "complete data; complete numbered representation"
    );
    let name = format!("control\n\u{202e}{}", "x".repeat(180));
    fs::write(dir.path().join(&name), "RIGHT").unwrap();
    let output = cell(
        &context,
        &format!(
            "read('crlf.txt', limit=1)\nread({})\ndone = True",
            serde_json::to_string(&name).unwrap()
        ),
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert_eq!(output.text.matches("RIGHT").count(), 1);
    assert_eq!(output.text.matches("crlf.txt").count(), 1);
    assert!(output.text.contains("[Bart read: ") && output.text.contains(r"\u202e"));
    assert!(!output.text.contains('\u{202e}'));
    assert!(
        output
            .text
            .lines()
            .filter(|line| line.starts_with('['))
            .all(|line| line.len() <= 80)
    );
    for code in [
        "read('missing')",
        "read('.')",
        "read('five.txt', limit=-1)",
        "read('five.txt', offset=True)",
        "read('')",
    ] {
        refused(&context, code).await;
    }
}

#[tokio::test]
async fn read_never_returns_truncated_or_lossy_data() {
    let dir = TempDir::new("helper-read-bounds");
    let context = files(dir.path());
    for (name, bytes) in [
        ("binary", b"a\0b".as_slice()),
        ("invalid", b"a\xffb".as_slice()),
    ] {
        fs::write(dir.path().join(name), bytes).unwrap();
        refused(&context, &format!("read({name:?})")).await;
    }
    fs::write(dir.path().join("exact"), vec![b'x'; 16 * 1024 * 1024]).unwrap();
    text(&context, "data = read('exact')\nassert len(data) == 16 * 1024 * 1024\nview = repr(data)\nassert view.endswith(data)\nprint('complete')", "complete").await;
    let mut file = fs::File::create(dir.path().join("huge")).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.write_all(b"small\n").unwrap();
    file.seek(SeekFrom::Start(64 * 1024 * 1024 + 1)).unwrap();
    file.write_all(b"tail\n").unwrap();
    refused(&context, "read('huge')").await;
    text(
        &context,
        "assert read('huge', limit=1) == 'small\\n'\nprint('complete range')",
        "complete range",
    )
    .await;
    for code in [
        "read('huge', offset=2, limit=1)",
        "read('huge', offset=3, limit=1)",
    ] {
        let output = cell(&context, code).await;
        assert!(
            output.is_error() && output.text.contains("limit"),
            "{}",
            output.text
        );
    }
    text(
        &context,
        r#"previous = 'unchanged'
try:
    previous = read('huge')
except BartError:
    pass
assert previous == 'unchanged'
os.mkfifo('fifo')
try:
    read('fifo')
except BartError:
    print('refused without blocking')"#,
        "refused without blocking",
    )
    .await;
}

#[tokio::test]
async fn text_expression_display_respects_python_structure_and_composition() {
    let dir = TempDir::new("helper-expression");
    fs::write(dir.path().join("a.txt"), "ALPHA").unwrap();
    fs::write(dir.path().join("b.txt"), "BETA").unwrap();
    let context = files(dir.path());
    let output = cell(
        &context,
        r#""module documentation"
from __future__ import annotations
alias = read
def decorate(cls):
    assert cls.__doc__ == 'class documentation'
    return cls
@decorate
class Example:
    "class documentation"
    def method(self, x: NotDefined):
        "method documentation"
        for name in ['a.txt', 'b.txt']:
            alias(name)
        return read('b.txt')
assert __doc__ == 'module documentation'
assert Example.method.__doc__ == 'method documentation'
assert Example.method.__annotations__['x'] == 'NotDefined'
result = Example().method(None)
assert result == 'BETA'
assert str(read('a.txt')) == 'ALPHA'
print('DONE')"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert_eq!(output.text.matches("[Bart read: a.txt:1-1]").count(), 1);
    assert_eq!(output.text.matches("[Bart read: b.txt:1-1]").count(), 1);
    assert_eq!(output.text.matches("BETA").count(), 1);
    let output = cell(
        &context,
        r#"def generate():
    "generator documentation"
    read('a.txt')
    yield read('b.txt')
    read('a.txt')
g = generate()
value = next(g)
assert value == 'BETA'
try:
    next(g)
except StopIteration:
    pass
print('DONE')"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert_eq!(output.text.matches("[Bart read: a.txt:1-1]").count(), 2);
    assert!(!output.text.contains("BETA"));
    let output = cell(
        &context,
        "def broken():\n    read('a.txt')\n    return 1 / 0\nbroken()\ndone = True",
    )
    .await;
    assert!(
        output.is_error()
            && output.text.contains("line 3")
            && output.text.contains("ZeroDivisionError"),
        "{}",
        output.text
    );
    let output = cell(&context, "Text = None").await;
    assert!(!output.is_error());
    let output = cell(&context, "read('a.txt')\ndone = True").await;
    assert!(
        !output.is_error() && output.text.contains("ALPHA"),
        "{}",
        output.text
    );
    let output = cell(&context, "import asyncio\nasync def asynchronous():\n    await asyncio.sleep(0)\n    read('a.txt')\n    return read('b.txt')\nvalue = asyncio.run(asynchronous())\nassert value == 'BETA'\nprint('DONE')").await;
    assert!(
        !output.is_error() && output.text.contains("ALPHA") && !output.text.contains("BETA"),
        "{}",
        output.text
    );
    text(&context, "Text('visible')\ndone = True", "visible").await;
    text(&context, "def data():\n    return Text('kept')\nvalue = data()\nassert value == 'kept'\nprint('silent')", "silent").await;
}

#[tokio::test]
async fn write_creates_or_replaces_keeping_line_endings_and_final_newlines() {
    let dir = TempDir::new("helper-write");
    let context = files(dir.path());
    let cases: [WriteCase; 10] = [
        ("new/nested/a.txt", None, "x\ny\n", b"x\ny\n"),
        ("b.txt", Some(b"old\n"), "x\ny\n", b"x\ny\n"),
        ("c.txt", Some(b"old\r\n"), "x\ny\n", b"x\r\ny\r\n"),
        ("d.bin", Some(b"\xFF\r\n"), "x\n", b"x\n"),
        ("e.txt", None, "x", b"x\n"),
        ("f.txt", None, "", b""),
        ("g.txt", Some(b""), "x", b"x\n"),
        ("h.txt", Some(b"old"), "new\n\r\n", b"new"),
        ("i.txt", Some(b"a\r\n"), "a\nb", b"a\r\nb\r\n"),
        ("j.txt", Some(b"same\n"), "same", b"same\n"),
    ];
    for (file, before, content, after) in cases {
        if let Some(before) = before {
            fs::write(dir.path().join(file), before).unwrap();
        }
        let output = cell(
            &context,
            &format!("assert write({}, {}) is None", py(file), py(content)),
        )
        .await;
        assert!(!output.is_error(), "{file}: {}", output.text);
        assert!(output.text.is_empty(), "{}", output.text);
        assert_eq!(fs::read(dir.path().join(file)).unwrap(), after, "{file}");
        assert_eq!(
            output.diffs.len(),
            usize::from(before.unwrap_or_default() != after),
            "{file}"
        );
        if let Some(diff) = output.diffs.first() {
            assert_eq!(diff.path, file);
            if before.is_some_and(|bytes| std::str::from_utf8(bytes).is_err()) {
                assert!(diff.hunks.contains("Binary"), "{}", diff.hunks);
                continue;
            }
            let shown_after: Vec<_> = diff
                .hunks
                .lines()
                .filter_map(|line| line.strip_prefix('+').or_else(|| line.strip_prefix(' ')))
                .collect();
            let shown_before: Vec<_> = diff
                .hunks
                .lines()
                .filter_map(|line| line.strip_prefix('-').or_else(|| line.strip_prefix(' ')))
                .collect();
            assert_eq!(
                shown_before,
                String::from_utf8_lossy(before.unwrap_or_default())
                    .lines()
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                shown_after,
                std::str::from_utf8(after)
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>()
            );
            assert!(
                !output.text.contains("@@"),
                "diffs stay out of model output"
            );
        }
    }
    assert_eq!(temporaries(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn edit_replaces_exactly_or_fails_without_writing() {
    let dir = TempDir::new("helper-edit");
    let context = files(dir.path());
    let cases: [EditCase; 13] = [
        (
            b"\xEF\xBB\xBFfn a() {}\nfn b() {}",
            "a()",
            "c()",
            false,
            Some(b"\xEF\xBB\xBFfn c() {}\nfn b() {}"),
        ),
        (
            b"one\r\ntwo\r\nthree\r\n",
            "one\ntwo",
            "1\n2",
            false,
            Some(b"1\r\n2\r\nthree\r\n"),
        ),
        (b"one\r\ntwo\nthree", "one\ntwo", "x", false, None),
        (b"x = 1;\nx = 1;\n", "x = 1;", "y", false, None),
        (b"aaa", "aa", "b", false, None),
        (
            b"x = 1;\nx = 1;\n",
            "x = 1;",
            "y = 2;",
            true,
            Some(b"y = 2;\ny = 2;\n"),
        ),
        (b"present", "absent", "x", false, None),
        (b"\xFFbinary", "binary", "x", false, None),
        (b"a\nb\n", "b\n", "c", false, Some(b"a\nc\n")),
        (b"a\nb", "b", "c\n", false, Some(b"a\nc")),
        (b"a\nb", "b", "c\n\n", false, Some(b"a\nc")),
        (b"a\r\nb\r\n", "b\n", "c", false, Some(b"a\r\nc\r\n")),
        (b"a\r\nb", "b", "c\n", false, Some(b"a\r\nc")),
    ];
    for (index, (before, old, new, all, after)) in cases.into_iter().enumerate() {
        let file = format!("{index}.txt");
        fs::write(dir.path().join(&file), before).unwrap();
        let code = format!(
            "assert edit({}, {}, {}, replace_all={}) is None",
            py(&file),
            py(old),
            py(new),
            if all { "True" } else { "False" }
        );
        let output = cell(&context, &code).await;
        assert_eq!(
            output.is_error(),
            after.is_none(),
            "case {index}: {}",
            output.text
        );
        assert_eq!(
            fs::read(dir.path().join(&file)).unwrap(),
            after.unwrap_or(before),
            "case {index}"
        );
        if after.is_some() {
            assert!(output.text.is_empty(), "{}", output.text);
        }
        assert_eq!(
            output.diffs.len(),
            usize::from(after.is_some()),
            "case {index}"
        );
        if let Some(diff) = output.diffs.first() {
            assert_eq!(diff.path, file);
            assert!(
                !output.text.contains("@@"),
                "diffs stay out of model output"
            );
        }
    }
    let amplified = cell(
        &context,
        &format!(
            "open('big', 'w').write('x' * 100_000)\nedit('big', 'x', {}, replace_all=True)",
            py(&"y".repeat(200))
        ),
    )
    .await;
    assert!(amplified.is_error(), "{}", amplified.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("big")).unwrap(),
        "x".repeat(100_000)
    );
}

#[tokio::test]
async fn edit_batches_match_the_original_and_report_where_they_fail() {
    let dir = TempDir::new("helper-edit-batch");
    let context = files(dir.path());
    let code_rs = dir.path().join("code.rs");
    fs::write(
        &code_rs,
        "fn one() {\n    log();\n}\nfn two() {\n    log();\n}\n",
    )
    .unwrap();
    let ambiguous = cell(&context, "edit('code.rs', '    log();', '    trace();')").await;
    assert!(ambiguous.is_error(), "{}", ambiguous.text);
    assert!(
        !cell(
            &context,
            "edit('code.rs', '    log();', '    trace();', within=(4, 6))"
        )
        .await
        .is_error()
    );
    assert_eq!(
        fs::read_to_string(&code_rs).unwrap(),
        "fn one() {\n    log();\n}\nfn two() {\n    trace();\n}\n"
    );
    fs::write(dir.path().join("f"), "α\r\nrepeat\r\nrepeat\r\nω\r\n").unwrap();
    let batch = cell(&context, "assert edit('f', edits=[{'old': 'repeat', 'new': 'α', 'replace_all': True}, ('α\\n', 'a much longer line\\n', (1, 1)), ('ω', '')]) is None").await;
    assert!(!batch.is_error(), "{}", batch.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("f")).unwrap(),
        "a much longer line\r\nα\r\nα\r\n\r\n"
    );
    let unchanged = fs::read_to_string(dir.path().join("f")).unwrap();
    for code in [
        "edit('f', 'α\\n', 'α\\r\\n', replace_all=True)",
        "edit('f', edits=[('a much', 'b'), ('absent', 'c')])",
        "edit('f', edits=[('a much', 'x'), ('much longer', 'y')])",
        "edit('f', 'α', 'b', within=(4, 4))",
        "edit('f', 'α', 'b', within=(0, 1))",
        "edit('f', edits=[])",
        "edit('f', edits=[('x', 'y')] * 1025)",
        "edit('missing/parent/f', 'x', 'y')",
    ] {
        refused(&context, code).await;
        assert_eq!(fs::read_to_string(dir.path().join("f")).unwrap(), unchanged);
    }
    assert!(!dir.path().join("missing").exists());
    fs::write(dir.path().join("many"), "x\n".repeat(100_001)).unwrap();
    let ambiguous = cell(&context, "edit('many', 'x', 'y')").await;
    assert!(ambiguous.is_error(), "{}", ambiguous.text);
    let excessive = cell(&context, "edit('many', 'x', 'y', replace_all=True)").await;
    assert!(excessive.is_error(), "{}", excessive.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("many")).unwrap(),
        "x\n".repeat(100_001)
    );
    let partial = cell(&context, "write('partial', 'one')\nedit('partial', 'one', 'two')\nedit('partial', 'two', 'three')\nedit('partial', 'missing', 'four')").await;
    assert!(partial.is_error(), "{}", partial.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("partial")).unwrap(),
        "three\n"
    );
    assert!(!partial.text.contains("Created ") && !partial.text.contains("Edited "));
    let effects = partial
        .text
        .split_once(&format!(
            "[Bart cell: cell[{}] changes made before the failure: ",
            partial.cell.unwrap()
        ))
        .unwrap()
        .1;
    assert_eq!(effects.matches("edited ").count(), 1, "{effects}");
}

#[tokio::test]
async fn background_notifications_match_once_and_complete() {
    use crate::history::NotificationKind;
    let dir = TempDir::new("job-notify");
    let context = session(dir.path(), Confinement::Unrestricted, 1000);
    let long = format!(
        "# {}\necho READY; while [ ! -e release ]; do sleep 0.01; done",
        "長".repeat(100)
    );
    for command in [
        "echo READY; echo READY; while [ ! -e release ]; do sleep 0.01; done; echo READY",
        "echo READY >&2; echo READY >&2; while [ ! -e release ]; do sleep 0.01; done; echo READY >&2",
        r"printf '\033[32mREADY\033[0m\n'; echo READY; while [ ! -e release ]; do sleep 0.01; done",
        long.as_str(),
    ] {
        let code = format!("job = bg_run([\"bash\", \"-c\", {command:?}], notify='^READY$')\njob");
        let output = cell(&context, &code).await;
        assert!(!output.is_error(), "{}", output.text);
        time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
            .await
            .unwrap();
        let notes = context.jobs.take_notifications();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(
            notes[0].kind,
            NotificationKind::JobMatched { id: output.jobs[0] }
        );
        assert!(notes[0].text.ends_with("\nREADY"), "{}", notes[0].text);
        assert_eq!(context.jobs.running_count(), 1);
        fs::write(dir.path().join("release"), "").unwrap();
        time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
            .await
            .unwrap();
        let notes = context.jobs.take_notifications();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let NotificationKind::JobFinished {
            command: original, ..
        } = &notes[0].kind
        else {
            panic!("expected terminal job notification");
        };
        assert_eq!(
            serde_json::from_str::<Vec<String>>(original).unwrap(),
            ["bash", "-c", command]
        );
        assert!(
            notes[0]
                .text
                .starts_with(&format!("[Bart job: job[{}] exit 0 after ", output.jobs[0]))
                && notes[0].text.contains("s; bash -c "),
            "{}",
            notes[0].text
        );
        fs::remove_file(dir.path().join("release")).unwrap();
    }
    for pattern in ["[", "(?=READY)", r"(x)\1"] {
        let output = cell(
            &context,
            &format!("bg_run([\"bash\", \"-c\", 'touch marker'], notify={pattern:?})"),
        )
        .await;
        assert!(output.is_error(), "{}", output.text);
    }
    assert!(!dir.path().join("marker").exists());
    assert_eq!(context.jobs.running_count(), 0);
}

#[tokio::test]
async fn compile_invalid_cells_never_execute_the_prefix() {
    let dir = TempDir::new("invalid-cell-preflight");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    for tail in ["await missing()", "(yield 1)", "return 1", "break"] {
        let output = cell(
            &context,
            &format!("write('must-not-exist', 'changed')\n{tail}"),
        )
        .await;
        assert!(
            output.is_error() && output.text.contains("SyntaxError"),
            "{}",
            output.text
        );
        assert!(
            !context.cwd.join("must-not-exist").exists(),
            "{}",
            output.text
        );
    }
    text(&context, "value=40\nvalue+2", "42").await;
}

#[test]
fn the_description_names_only_what_the_session_has_and_its_interface_is_python() {
    let servers = ["tracker".to_owned(), "docs".to_owned()];
    for escape in [false, true] {
        for web_search in [false, true] {
            for mcp_servers in [&[][..], &servers[..]] {
                let one_turn = escape != web_search;
                let text = super::definition(&super::Surface {
                    python: "Python 3.12.3 (/usr/bin/python3)",
                    escape,
                    web_search,
                    mode: if one_turn {
                        crate::tool::TurnMode::Prompt(crate::tool::BackgroundJobs::Stop)
                    } else {
                        crate::tool::TurnMode::Session
                    },
                    mcp_servers,
                })
                .description;
                let case = format!(
                    "escape={escape} web_search={web_search} mcp={}",
                    mcp_servers.len()
                );
                assert_eq!(text.contains("sandbox: bool"), escape, "{case}");
                assert_eq!(text.contains("build.wait() instead"), one_turn, "{case}");
                assert_eq!(text.contains("def wait(self)"), one_turn, "{case}");
                assert_eq!(
                    text.contains("the message resumes you"),
                    !one_turn,
                    "{case}"
                );
                assert_eq!(text.contains("web_search"), web_search, "{case}");
                assert_eq!(text.contains("mcp"), !mcp_servers.is_empty(), "{case}");
                let stub = text
                    .split_once("```python\n")
                    .and_then(|(_, rest)| rest.split_once("```"))
                    .expect("one Python block")
                    .0;
                let mut parse = std::process::Command::new("python3")
                    .args([
                        "-I",
                        "-c",
                        "import ast, sys; ast.parse(sys.stdin.read(), feature_version=(3, 10))",
                    ])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                std::io::Write::write_all(&mut parse.stdin.take().unwrap(), stub.as_bytes())
                    .unwrap();
                assert!(
                    parse.wait().unwrap().success(),
                    "{case}: the stub is not Python 3.10:\n{stub}"
                );
            }
        }
    }
}

#[tokio::test]
async fn job_wait_exists_only_in_one_turn_sessions() {
    for one_turn in [false, true] {
        let dir = TempDir::new("python-job-wait");
        let context = session(dir.path(), Confinement::Unrestricted, 10_000);
        context.set_turn_mode(if one_turn {
            crate::tool::TurnMode::Prompt(crate::tool::BackgroundJobs::Stop)
        } else {
            crate::tool::TurnMode::Session
        });
        let code = if one_turn {
            "j = bg_run(['true'])\nassert hasattr(j, 'wait')\nassert j.wait().return_code == 0"
        } else {
            "j = bg_run(['true'])\nassert not hasattr(j, 'wait')"
        };
        let output = cell(&context, code).await;
        assert!(!output.is_error(), "{}", output.text);
    }
}

#[tokio::test]
async fn full_job_registry_refuses_foreground_before_spawning_and_recovers() {
    let dir = TempDir::new("python-full-jobs");
    let context = session(dir.path(), Confinement::Unrestricted, 10000);
    let result = cell(
        &context,
        "jobs = [bg_run([\"bash\", \"-c\", 'sleep 30']) for _ in range(16)]\nrun([\"bash\", \"-c\", 'touch must-not-exist'])",
    )
    .await;
    assert!(
        result.is_error() && result.text.contains("16 background jobs"),
        "{}",
        result.text
    );
    assert!(!dir.path().join("must-not-exist").exists());
    assert_eq!(context.jobs.running_count(), 16);
    let ended = cell(&context, "for j in jobs: j.kill()\nprint('stopped')").await;
    assert!(
        !ended.is_error() && ended.text.ends_with("stopped"),
        "{}",
        ended.text
    );
    assert_eq!(context.jobs.running_count(), 0);
}

#[tokio::test]
async fn restarting_python_kills_its_owned_descendants() {
    let dir = TempDir::new("python-restart-tree");
    let context = session(dir.path(), Confinement::Unrestricted, 10000);
    let result = cell(&context, r#"import subprocess, sys
p = subprocess.Popen([sys.executable, '-c', "import os,time; open('pid','w').write(str(os.getpid())); time.sleep(30)"], start_new_session=sys.platform == 'linux', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
while not pathlib.Path('pid').exists(): time.sleep(.01)
print('ready')"#).await;
    assert!(
        !result.is_error() && result.text.ends_with("ready"),
        "{}",
        result.text
    );
    let pid = fs::read_to_string(dir.path().join("pid")).unwrap();
    let restarted = call(
        &json!({"code": "print('fresh')", "restart": true}),
        &context,
    )
    .await;
    assert!(
        !restarted.is_error() && restarted.text[..restarted.prefix_len.unwrap()].ends_with("fresh"),
        "{}",
        restarted.text
    );
    wait_until_dead(pid.trim()).await;
}

#[tokio::test]
async fn cancelling_an_unobserved_foreground_result_stops_its_adopted_descendants() {
    let dir = TempDir::new("python-shell-handoff-cancel");
    let context = session(dir.path(), Confinement::Unrestricted, 10000);
    let live = context.live.start("cancel", "", None);
    let request = crate::tool::execution::Request::parse(
        json!({"argv": ["bash", "-c", "sleep 30 & echo $! > pid"], "timeout": 10}),
        crate::tool::Escape::Withheld,
    )
    .unwrap();
    let mut pending = super::helpers::start_program(
        &request,
        crate::tool::output::Name::Process { cell: 1, call: 1 },
        &context,
        live.progress(),
    )
    .await
    .unwrap();
    let result = (&mut pending.task).await.unwrap();
    assert_eq!(result.code, 0);
    assert!(result.adopted.is_some(), "{result:?}");
    // The interpreter never consumed the completed worker's response.
    drop(pending);
    context.jobs.settled().await;
    wait_until_dead(fs::read_to_string(dir.path().join("pid")).unwrap().trim()).await;
    assert_eq!(context.jobs.running_count(), 0);
}

#[tokio::test]
async fn earlier_cells_processes_and_jobs_are_files_by_number_across_restarts() {
    let dir = TempDir::new("python-saved");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let first = cell(
        &context,
        "run([\"bash\", \"-c\", 'echo one']); print(run([\"bash\", \"-c\", 'echo two']).stdout, end='')",
    )
    .await;
    let number = first.cell.unwrap();
    let started = cell(
        &context,
        "print(bg_run([\"bash\", \"-c\", 'echo jobbed']).id)",
    )
    .await;
    context.jobs.settled().await;
    let restarted = call(
        &json!({"restart": true, "code": format!(
            "assert 'echo one' in read(cell[{number}].code)\nassert read(cell[{number}].run[2]) == 'two\\n'\nsaved = read(cell[{number}].output)\nassert 'one' in saved and 'two' in saved\nassert 'jobbed' in job[{}].stdout\nprint('artifacts recovered')", started.jobs[0])}),
        &context,
    ).await;
    assert!(
        !restarted.is_error() && restarted.text.contains("artifacts recovered"),
        "{}",
        restarted.text
    );
    for missing in [
        "cell[999]",
        "cell[1].run[9]",
        "image[1]",
        "paste[1]",
        "cell[0]",
    ] {
        refused(&context, missing).await;
    }
    cell(&context, "cell = 5").await;
    text(&context, "print(cell)", "5").await;
}

#[tokio::test]
async fn a_call_that_only_waited_for_a_busy_cell_is_not_numbered_as_one() {
    let dir = TempDir::new("python-busy");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let busy = call(
        &json!({"code": "import signal, time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\ntime.sleep(60)", "timeout": 1}),
        &context,
    )
    .await;
    assert!(busy.text.contains("is still running"), "{}", busy.text);
    let waited = call(&json!({"code": "print('never')", "timeout": 1}), &context).await;
    assert_eq!(
        waited.text,
        "[Bart cell: cell[1] output continues]\n[Bart not run: cell[1] is still running after 1s more; call again to keep waiting, or pass restart: true]"
    );
    assert_eq!(
        waited.cell, None,
        "its code never ran, so nothing may point at it"
    );
    let after = call(&json!({"code": "print('ran')", "restart": true}), &context).await;
    let ran = after.cell.unwrap();
    assert!(ran > busy.cell.unwrap());
    text(
        &context,
        &format!("print(read(cell[{ran}].code))"),
        "print('ran')",
    )
    .await;
}

#[tokio::test]
async fn an_adopted_foreground_job_is_reachable_and_numbered_as_a_job() {
    let dir = TempDir::new("python-adopted");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let adopted = cell(
        &context,
        "r = run([\"bash\", \"-c\", 'echo before; (sleep 0.3; echo after) & exit 0']); print(r.stdout_file.path.read_text())",
    )
    .await;
    let id = *adopted
        .jobs
        .first()
        .unwrap_or_else(|| panic!("{}", adopted.text));
    context.jobs.settled().await;
    text(
        &context,
        &format!("j = job[{id}]\nassert j.stdout == 'before\\nafter\\n' and j.stderr == ''\nassert j.outcome == {{'exit':0}} and j\nassert j.stdout_file.path == r.stdout_file.path\nt = repr(j); print('before' in t and 'after' in t)"),
        "True",
    )
    .await;
    let reopened = crate::tool::output::Storage::open(context.artifacts.dir()).unwrap();
    assert_eq!(
        reopened.highest_job(),
        id,
        "a reopened session numbers after it"
    );
}

#[tokio::test]
async fn showing_a_saved_image_again_names_it_without_its_path() {
    let dir = TempDir::new("python-image-again");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    fs::write(
        context.artifacts.dir().join("image-3.png"),
        crate::history::test_support::PNG,
    )
    .unwrap();
    let mut shown = cell(&context, "display(image[3])").await;
    let saved = context.artifacts.dir().to_string_lossy().into_owned();
    assert!(!shown.text.contains(&saved), "{}", shown.text);
    assert_eq!(shown.images.len(), 1);
    assert_eq!(shown.images[0].label, "[Bart image: image[3]]");
    assert!(shown.text.is_empty(), "{}", shown.text);
    let images = crate::tool::deliver(&mut shown, &context, 8, true).await;
    assert_eq!(
        fs::read(&images[0].path).unwrap(),
        crate::history::test_support::PNG
    );
    let shown = call(
        &json!({"restart":true,"code":format!("display(image[{}])", images[0].number)}),
        &context,
    )
    .await;
    assert_eq!(shown.images[0].bytes(), crate::history::test_support::PNG);
    assert!(!shown.text.contains("will be shown"));
}

#[tokio::test]
async fn a_busy_cell_keeps_its_number_and_shell_order_while_the_next_call_waits() {
    let dir = TempDir::new("python-busy-processes");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let busy = call(
        &json!({"code": "import signal, time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\nrun([\"bash\", \"-c\", 'echo first'])\ntime.sleep(6)\nprint(run([\"bash\", \"-c\", 'echo second']).stdout, end='')", "timeout": 1}),
        &context,
    )
    .await;
    let first = busy.cell.unwrap();
    assert!(busy.text.contains("is still running"), "{}", busy.text);
    let next = call(&json!({"code": "print('next')", "timeout": 20}), &context).await;
    assert!(
        next.text[..next.prefix_len.unwrap()].contains("second")
            && next
                .text
                .contains(&format!("[Bart cell: cell[{}] result]", first + 1))
            && next.text[..next.prefix_len.unwrap()].ends_with("next"),
        "{}",
        next.text
    );
    assert_eq!(
        next.cell,
        Some(first + 1),
        "waiting allocates no other cell"
    );
    text(
        &context,
        &format!("print(read(cell[{first}].run[2]) + read(cell[{first}].output).splitlines()[-1])"),
        "second\nsecond",
    )
    .await;
}

#[tokio::test]
async fn a_foreground_job_that_cannot_get_its_log_is_not_adopted() {
    let dir = TempDir::new("python-adopt-full");
    let spill = dir.path().join("spill");
    fs::create_dir_all(&spill).unwrap();
    for n in 0..16_384 - 3 {
        fs::write(spill.join(format!("filler-{n}")), "").unwrap();
    }
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let result = cell(
        &context,
        r#"r=run(["bash", "-c", '(sleep 0.3; echo after; touch completed) & exit 0'])
assert pathlib.Path('completed').exists()
assert r.stdout_file.incomplete is not None
print('after')"#,
    )
    .await;
    assert!(result.jobs.is_empty(), "{}", result.text);
    assert!(
        result.text.contains("after"),
        "the foreground call waited for its descendant: {}",
        result.text
    );
}

#[tokio::test]
async fn a_matched_line_notice_stays_short_however_long_the_line() {
    let dir = TempDir::new("python-match-clip");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    cell(
        &context,
        "bg_run([\"bash\", \"-c\", \"printf 'x%.0s' $(seq 20000); echo\"], notify='x')",
    )
    .await;
    context.jobs.settled().await;
    let matched = context
        .jobs
        .take_notifications()
        .into_iter()
        .find(|note| {
            matches!(
                note.kind,
                crate::history::NotificationKind::JobMatched { .. }
            )
        })
        .unwrap();
    assert!(matched.text.chars().count() < 300, "{}", matched.text);
    assert!(matched.text.ends_with(" more chars]"), "{}", matched.text);
}

#[tokio::test]
async fn every_name_the_stub_defines_exists_in_cells_with_its_parameters() {
    let dir = TempDir::new("python-stub-drift");
    let context = session(dir.path(), Confinement::Unrestricted, 1_000_000);
    let description = super::definition(&super::Surface {
        python: "Python 3",
        escape: false,
        web_search: false,
        mode: crate::tool::TurnMode::Session,
        mcp_servers: &["fx".to_owned()],
    })
    .description;
    let stub = description
        .split_once("```python\n")
        .and_then(|(_, rest)| rest.split_once("```"))
        .unwrap()
        .0;
    let check = format!(
        r#"
import ast, inspect
problems = []
def params(node):
    a = node.args
    names = [x.arg for x in a.posonlyargs + a.args] + (["*" + a.vararg.arg] if a.vararg else []) + [x.arg for x in a.kwonlyargs] + (["**" + a.kwarg.arg] if a.kwarg else [])
    return names, [ast.literal_eval(d) for d in a.defaults + [d for d in a.kw_defaults if d is not None]]
declared = {{}}
body = ast.parse({stub:?}).body
private = {{node.name: node for node in body if isinstance(node, ast.ClassDef) and node.name.startswith("_")}}
for node in body:
    if isinstance(node, ast.FunctionDef):
        declared.setdefault(node.name, []).append(node)
    elif isinstance(node, ast.AnnAssign) and getattr(node.annotation, "id", None) in private:
        for member in private[node.annotation.id].body:
            if isinstance(member, ast.FunctionDef):
                name = node.target.id if member.name == "__call__" else f"{{node.target.id}}.{{member.name}}"
                member.args.posonlyargs = [a for a in member.args.posonlyargs if a.arg != "self"]
                member.args.args = [a for a in member.args.args if a.arg != "self"]
                declared.setdefault(name, []).append(member)
    elif isinstance(node, ast.ClassDef) and not node.name.startswith("_"):
        try:
            runtime = eval(node.name)
        except NameError:
            problems.append(f"class {{node.name}} is missing")
            continue
        if node.name == "Edit":
            continue
        init = inspect.signature(runtime.__init__).parameters if "__init__" in vars(runtime) else {{}}
        for member in node.body:
            name = getattr(member, "name", None) or getattr(getattr(member, "target", None), "id", None)
            if name and not name.startswith("_") and not hasattr(runtime, name) and name not in init:
                problems.append(f"{{node.name}}.{{name}} is missing")
    elif isinstance(node, ast.AnnAssign):
        try:
            eval(node.target.id)
        except NameError:
            problems.append(f"{{node.target.id}} is missing")
for name, nodes in declared.items():
    try:
        runtime = inspect.signature(eval(name)).parameters.values()
    except NameError:
        problems.append(f"{{name}} is missing")
        continue
    names = [{{p.VAR_POSITIONAL: "*", p.VAR_KEYWORD: "**"}}.get(p.kind, "") + p.name for p in runtime]
    defaults = [p.default for p in runtime if p.default is not p.empty]
    stub_names = sorted({{n for node in nodes for n in params(node)[0]}})
    if sorted(names) != stub_names:
        problems.append(f"{{name}}: runtime {{names}} vs stub {{stub_names}}")
    if len(nodes) == 1 and params(nodes[0])[1] != defaults:
        problems.append(f"{{name}}: runtime defaults {{defaults}} vs stub {{params(nodes[0])[1]}}")
print(problems)
"#
    );
    text(&context, &check, "[]").await;
}

#[tokio::test]
async fn interrupted_mcp_startup_waits_dispatch_nothing_and_leave_startup_running() {
    let dir = TempDir::new("python-mcp-starting");
    let gate = dir.path().join("gate");
    let calls = dir.path().join("calls");
    let config = crate::mcp::Config::parse(
        &json!({"mcpServers":{"fx":{
            "command":"python3", "args":["-c",include_str!("../../../mcp/fixtures/server.py")],
            "env":{"FIXTURE_START_GATE":gate,"FIXTURE_CALL_FILE":calls}
        }}})
        .to_string(),
    )
    .unwrap();
    let registry = crate::mcp::Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), Confinement::Unrestricted),
        &reqwest::Client::new(),
        &crate::auth::Store::new(dir.path().join("auth.json")),
    );
    let context = Arc::new(
        Context::new(
            dir.path().to_path_buf(),
            crate::tool::test_support::executable(),
            dir.path().join("spill"),
            10_000,
            Confinement::Unrestricted,
            Arc::clone(&registry),
        )
        .unwrap(),
    );
    let interrupted = call(&json!({"code":"mcp('fx','echo')", "timeout":1}), &context).await;
    assert!(interrupted.is_error());
    assert!(
        !calls.exists(),
        "a cancelled readiness wait dispatches no tool"
    );
    assert_eq!(registry.servers()[0]["status"], "starting");
    fs::write(gate, "release").unwrap();
    for args in ["image='not-base64'", "mime='image/jpeg'"] {
        let shown = cell(&context, &format!("mcp('fx','mixed', {args})")).await;
        assert!(
            !shown.is_error() && shown.images.is_empty() && shown.text.contains("not shown"),
            "{}",
            shown.text
        );
    }
    assert_eq!(registry.servers()[0]["status"], "ready");
    assert_eq!(
        fs::read_to_string(calls).unwrap().lines().count(),
        2,
        "no replay on presentation failure"
    );
}

#[tokio::test]
async fn writable_targets_preserve_change_and_replacement_detection() {
    let dir = TempDir::new("python-target-races");
    let context = session(dir.path(), Confinement::Unrestricted, 2000);
    let output = cell(
        &context,
        r#"
Target = write.__globals__['_Target']
for mode in ('modify', 'replace', 'metadata'):
    pathlib.Path('target').write_text('original')
    with Target('target', False) as target:
        assert target.read() == b'original'
        if mode == 'modify':
            pathlib.Path('target').write_text('external')
        elif mode == 'replace':
            pathlib.Path('replacement').write_text('external')
            os.replace('replacement', 'target')
        else:
            os.chmod('target', 0o600)
        try:
            target.commit(b'unsafe', 'probe')
        except BartError as error:
            assert 'changed' in str(error), str(error)
        else:
            raise AssertionError('concurrent change was accepted: ' + mode)
        assert pathlib.Path('target').read_text() != 'unsafe'
print('changes refused')
"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(output.text.contains("changes refused"));
}

#[tokio::test]
async fn writes_and_edits_reach_every_hard_link_and_keep_the_mode() {
    let dir = TempDir::new("python-hard-link");
    let context = session(dir.path(), Confinement::Unrestricted, 2000);
    text(
        &context,
        "import os, stat\n\
         open('a', 'w').write('old\\n'); os.chmod('a', 0o640); os.link('a', 'b')\n\
         write('b', 'new\\n'); edit('b', 'new', 'newer')\n\
         print(open('a').read().strip(), oct(stat.S_IMODE(os.stat('a').st_mode)), os.stat('a').st_nlink)",
        "newer 0o640 2",
    )
    .await;
}

#[tokio::test]
async fn root_exit_wakes_the_model_without_finishing_or_releasing_inherited_descendants() {
    use crate::history::{JobOutcome, NotificationKind};
    for (end, expected, kill) in [
        ("exit 0", JobOutcome::Exit(0), false),
        ("exit 7", JobOutcome::Exit(7), false),
        ("kill -TERM $$", JobOutcome::Signal(15), false),
        ("exit 0", JobOutcome::Exit(0), true),
    ] {
        let dir = TempDir::new("background-root-exit");
        let context = session(dir.path(), Confinement::Unrestricted, 1000);
        let command = format!(
            "(while [ ! -e release ]; do sleep .01; done) >/dev/null 2>&1 & echo $! > pid2; (while [ ! -e release ]; do sleep .01; done; echo descendant-finished) >/dev/null 2>&1 & echo $! > pid; echo built > receipt; {end}"
        );
        let output = cell(
            &context,
            &format!("j=bg_run([\"bash\", \"-c\", {command:?}]); print(j.id)"),
        )
        .await;
        assert!(!output.is_error(), "{}", output.text);
        time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
            .await
            .unwrap();
        let notes = context.jobs.take_notifications();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            matches!(notes[0].kind, NotificationKind::JobCommandExited { outcome, .. } if outcome == expected)
        );
        assert!(notes[0].terminal_job().is_none());
        assert_eq!(
            notes[0].text.as_ref(),
            format!(
                "[Bart job: job[{}] command {}; child processes still run]",
                output.jobs[0],
                expected.label()
            )
        );
        assert_eq!(context.jobs.running_count(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("receipt")).unwrap(),
            "built\n"
        );
        let pid = fs::read_to_string(dir.path().join("pid")).unwrap();
        if !kill {
            fs::write(dir.path().join("release"), "").unwrap();
        } else {
            assert!(!cell(&context, "j.kill()").await.is_error());
        }
        time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
            .await
            .unwrap();
        let notes = context.jobs.take_notifications();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            matches!(notes[0].kind, NotificationKind::JobFinished { outcome, .. } if outcome == if kill { JobOutcome::Killed } else { expected })
        );
        assert_eq!(context.jobs.running_count(), 0);
        wait_until_dead(pid.trim()).await;
        wait_until_dead(fs::read_to_string(dir.path().join("pid2")).unwrap().trim()).await;
    }
}

#[tokio::test]
async fn python_mutations_share_one_net_diff_including_failed_cells() {
    let dir = TempDir::new("python-native-diff");
    fs::write(dir.path().join("file"), "original\n").unwrap();
    let context = files(dir.path());
    let output = cell(
        &context,
        r#"from pathlib import Path
import io
assert isinstance(open('file'), io.TextIOWrapper)
Path('file').write_text('intermediate\n')
edit('file', 'intermediate', 'helper')
with io.open('file', 'a') as f: f.write('tail\n')
raise RuntimeError('after writing')"#,
    )
    .await;
    assert!(output.is_error());
    assert_eq!(
        fs::read_to_string(dir.path().join("file")).unwrap(),
        "helper\ntail\n"
    );
    assert_eq!(output.diffs.len(), 1);
    let hunks = &output.diffs[0].hunks;
    assert!(
        hunks.contains("-original") && hunks.contains("+helper") && hunks.contains("+tail"),
        "{hunks}"
    );
    assert!(!hunks.contains("intermediate"));
    assert!(!output.text.contains("@@"));
    let reverted = cell(
        &context,
        r#"Path('file').write_text('transient\n'); Path('file').write_text('helper\ntail\n')"#,
    )
    .await;
    assert!(reverted.diffs.is_empty());
    assert!(
        !cell(&context, "held = open('file', 'r+'); held.seek(0)")
            .await
            .is_error()
    );
    let retained = cell(
        &context,
        r#"held.write('retained\n'); held.truncate(); held.flush()"#,
    )
    .await;
    assert_eq!(retained.diffs.len(), 1);
    assert!(retained.diffs[0].hunks.contains("+retained"));
    let closed = cell(&context, "held.close()").await;
    assert!(closed.diffs.is_empty());
}

#[tokio::test]
async fn python_replace_delete_binary_and_oversize_tracking_preserve_operations() {
    let dir = TempDir::new("python-diff-operations");
    fs::write(dir.path().join("destination"), "old\n").unwrap();
    fs::write(dir.path().join("delete"), "removed\n").unwrap();
    fs::write(dir.path().join("large"), vec![b'x'; 5 * 1024 * 1024]).unwrap();
    let context = files(dir.path());
    let output = cell(
        &context,
        r#"from pathlib import Path
import os
Path('temporary').write_text('new\n')
os.replace('temporary', 'destination')
Path('delete').unlink()
Path('binary').write_bytes(b'\x00\xff')
with open('large','a') as f: f.write('tail')"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("destination")).unwrap(),
        "new\n"
    );
    assert!(!dir.path().join("delete").exists());
    assert!(!dir.path().join("temporary").exists());
    assert_eq!(fs::read(dir.path().join("binary")).unwrap(), b"\x00\xff");
    assert_eq!(
        fs::metadata(dir.path().join("large")).unwrap().len(),
        5 * 1024 * 1024 + 4
    );
    let by_path: std::collections::HashMap<_, _> = output
        .diffs
        .iter()
        .map(|d| (d.path.as_str(), d.hunks.as_str()))
        .collect();
    assert!(by_path["destination"].contains("-old") && by_path["destination"].contains("+new"));
    assert!(by_path["delete"].contains("-removed"));
    assert!(!by_path.contains_key("temporary"));
    assert!(by_path["binary"].contains("Binary"));
    assert!(by_path["large"].contains("budget"));
    let shell = cell(
        &context,
        "run([\"bash\", \"-c\", \"printf 'subprocess\\n' > destination\"])",
    )
    .await;
    assert!(!shell.is_error(), "{}", shell.text);
    assert!(
        shell.diffs.is_empty(),
        "subprocess output is not Python file tracking"
    );
    let refused = cell(&context, "open('destination', 'x')").await;
    assert!(refused.is_error() && refused.diffs.is_empty());
}

#[tokio::test]
async fn python_diff_limits_never_block_writes_and_buffers_are_not_forced_to_flush() {
    let dir = TempDir::new("python-diff-limits");
    let context = files(dir.path());
    let pending = cell(
        &context,
        "held = open('buffered', 'w'); held.write('pending')",
    )
    .await;
    assert!(!pending.is_error(), "{}", pending.text);
    assert_eq!(fs::read(dir.path().join("buffered")).unwrap(), b"");
    let flushed = cell(&context, "held.flush(); held.close()").await;
    assert_eq!(fs::read(dir.path().join("buffered")).unwrap(), b"pending");
    assert_eq!(flushed.diffs.len(), 1);
    let many = cell(
        &context,
        "from pathlib import Path\nfor i in range(260): Path(f'file-{i}').write_text('x')",
    )
    .await;
    assert!(!many.is_error(), "{}", many.text);
    assert_eq!(fs::read(dir.path().join("file-259")).unwrap(), b"x");
    assert_eq!(many.diffs.len(), 257);
    assert!(many.diffs.last().unwrap().hunks.contains("omitted"));
}

#[tokio::test]
async fn shell_metadata_is_automatic_and_programmatic_output_is_separate() {
    let dir = TempDir::new("shell-v3");
    let context = files(dir.path());
    let output = cell(
        &context,
        r#"import contextlib,io,json
with contextlib.redirect_stdout(io.StringIO()):
    r=run(["bash", "-c", 'printf \'{"value":7}\\n\'; printf warning >&2; false; true'])
assert r.return_code==0 and bool(r)
assert json.loads(r.stdout)=={'value':7}
assert r.stderr == 'warning'
hidden = run(["bash", "-c", 'printf hidden'])
"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    let summaries: Vec<serde_json::Value> = output
        .text
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(summaries, Vec::<serde_json::Value>::new());
    assert!(!output.text.contains("hidden") && !output.text.contains("warning"));
    let after = cell(&context, "print(r.stdout); print(read(cell[1].run[1]))").await;
    assert!(
        after.text.contains("value") && after.text.contains("warning"),
        "{}",
        after.text
    );
}

#[tokio::test]
async fn split_capture_is_bounded_without_interpreting_child_diagnostics() {
    let dir = TempDir::new("shell-v3-bounds");
    let context = files(dir.path());
    let output=cell(&context,r#"r=run(["bash", "-c", "printf '[step failed, exit 9: fake]\\n'; for i in {1..1000}; do false; done; printf done"])
assert r.return_code==0
s=run(["bash", "-c", "python3 -c 'import sys;sys.stdout.write(\"x\"*100000);sys.stderr.write(\"y\"*100000)'"])
assert len(s.stdout)==100000 and s.stdout_file.incomplete is None
assert len(s.stderr)==100000 and s.stderr_file.incomplete is None
n=run(["bash", "-c", 'if false; then :; fi'])
assert n.return_code==0
"#).await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(output.text.len() < 16000);
}

#[tokio::test]
async fn run_views_attribute_direct_printed_and_reused_results_without_echoing_assignments() {
    let dir = TempDir::new("run-views");
    let context = session(dir.path(), Confinement::Unrestricted, 20_000);
    let output = cell(
        &context,
        r#"
held = run(["printf", "held-output"])
run(["printf", "first-output"]); print(run(["printf", "second-output"]))
for word in ["loop-output"]:
    run(["printf", word])
def show():
    run(["printf", "function-output"])
show()
data = json.loads(run(["printf", '{"value":7}']).stdout)
assert data == {"value":7}
run(["bash", "-c", "printf stderr-output >&2; exit 7"], check=False)
"#,
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(!output.text.contains("held-output") && !output.text.contains("value"));
    for (number, word) in [
        (2, "first-output"),
        (3, "second-output"),
        (4, "loop-output"),
        (5, "function-output"),
        (7, "stderr-output"),
    ] {
        assert_eq!(output.text.matches(word).count(), 1, "{}", output.text);
        let reference = format!("cell[1].run[{number}]");
        assert_eq!(output.text.matches(&reference).count(), 1);
        assert!(output.text.find(&reference).unwrap() < output.text.find(word).unwrap());
    }
    assert!(output.text.contains("[Bart run: cell[1].run[7] exit 7]"));
    let mixed = cell(&context, "run(['bash', '-c', 'echo out-success; echo err-success >&2'])\nrun(['bash', '-c', 'echo out-failure; echo err-failure >&2; exit 2'], check=False)").await;
    assert!(!mixed.is_error(), "{}", mixed.text);
    for marker in ["out-success", "err-success", "out-failure", "err-failure"] {
        assert_eq!(mixed.text.matches(marker).count(), 1, "{}", mixed.text);
    }
    let later = cell(&context, "held").await;
    assert!(later.text.contains("cell[1].run[1]") && later.text.contains("held-output"));
    let empty = cell(&context, "run(['true'])").await;
    assert!(empty.text.starts_with("[Bart run: cell[4].run[1] exit 0]"));
    assert!(!empty.text.contains("full") && !empty.text.contains("[no output]"));
}

#[tokio::test]
async fn run_views_use_the_cell_budget_and_distinguish_incomplete_capture_from_truncation() {
    let dir = TempDir::new("run-view-bounds");
    let context = session(dir.path(), Confinement::Unrestricted, 32 * 1024);
    let output = cell(&context, r#"
r=run([sys.executable, '-c', "import sys;sys.stdout.write('x'*20000+'END');sys.stderr.buffer.write(bytes([255]))"])
r
assert len(r.stdout) == 20003
assert r.stderr_file.path.read_bytes() == bytes([255])
try:
    r.stderr
except BartError:
    pass
else:
    raise AssertionError('invalid UTF-8 was accepted')
"#).await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(
        output.text.contains("[Bart run: cell[1].run[1] exit 0]"),
        "{}",
        &output.text[..output.text.len().min(300)]
    );
    assert!(output.text.contains("END"));
    let binary = cell(
        &context,
        "run([sys.executable, '-c', 'import sys;sys.stdout.buffer.write(bytes([255]))'])",
    )
    .await;
    assert!(!binary.is_error() && binary.text.contains('�'));
    let recovered = call(&json!({"restart": true, "code": "assert cell[1].stdout[1].read_bytes() == b'x' * 20000 + b'END'\nassert cell[1].stderr[1].read_bytes() == bytes([255])"}), &context).await;
    assert!(!recovered.is_error(), "{}", recovered.text);
    assert!(
        !output
            .text
            .contains(&context.spill_dir.to_string_lossy().to_string())
    );
    assert!(output.text.len() > 20_000 && output.text.len() < 32 * 1024);
    let partial = cell(
        &context,
        r#"
p=run(['bash', '-c', 'echo partial-output; sleep 30 & exit 0'])
p
job[p.retained_job].kill()
"#,
    )
    .await;
    assert!(!partial.is_error(), "{}", partial.text);
    assert!(partial.text.contains("partial-output") && partial.text.contains("incomplete capture"));
    let abnormal = cell(
        &context,
        r#"
try:
    run(['bash', '-c', 'echo before-signal; kill -TERM $$'])
except RunError as error:
    error.result
"#,
    )
    .await;
    assert!(!abnormal.is_error(), "{}", abnormal.text);
    assert!(abnormal.text.contains("before-signal") && abnormal.text.contains("signal"));
}

#[tokio::test]
async fn shell_raw_files_recover_exact_data_without_repeating_effects() {
    let dir = TempDir::new("shell-raw");
    let context = files(dir.path());
    let output = cell(&context, r#"
r=run(["bash", "-c", "printf x >> effects; python3 -c 'import json,sys; print(json.dumps(list(range(20000)))); sys.stderr.buffer.write(bytes(range(256))*200)'"])
assert json.loads(r.stdout) == list(range(20000))
try:
    r.stderr
except BartError as error:
    assert 'not UTF-8' in str(error)
else:
    raise AssertionError('binary stderr was silently decoded')
assert r.stdout_file.incomplete is None and r.stderr_file.incomplete is None
assert json.loads(read(r.stdout_file.path)) == list(range(20000))
assert r.stderr_file.path.read_bytes() == bytes(range(256))*200
assert read('effects') == 'x'
s=run(["bash", "-c", "printf '\\033[31mred\\033[0m\\rgreen\\n' >&2; printf '\\377\\000a'"])
assert s.stdout_file.path.read_bytes() == b'\xff\x00a'
assert s.stderr_file.path.read_bytes() == b'\x1b[31mred\x1b[0m\rgreen\n'
assert '\x1b' in s.stderr
saved = r.stdout_file.path
r.stdout_file.incomplete = 'capture still open'
try:
    r.stdout
except BartError as error:
    assert 'incomplete' in str(error) and 'stdout_file.path' in str(error)
else:
    raise AssertionError('incomplete capture was returned')
r.stdout_file.incomplete = None
r.stdout_file.path = pathlib.Path('oversize').resolve()
with r.stdout_file.path.open('wb') as file:
    file.truncate(64*1024*1024+1)
try:
    r.stdout
except BartError as error:
    assert '64 MiB' in str(error)
else:
    raise AssertionError('oversize capture was returned')
"#).await;
    assert!(!output.is_error(), "{}", output.text);
    text(&context, "print(len(json.loads(read(saved))))", "20000").await;
}

#[tokio::test]
async fn missing_program_names_the_executable_and_does_not_claim_cleanup_failed() {
    let dir = TempDir::new("python-missing-program");
    let context = session(dir.path(), Confinement::Unrestricted, 2000);
    let output = cell(&context, "run(['bart-test-nonexistent-executable-93c1'])").await;
    assert!(output.is_error());
    assert!(
        output
            .text
            .contains("bart-test-nonexistent-executable-93c1"),
        "{}",
        output.text
    );
    assert!(output.text.contains("exit 127"), "{}", output.text);
    assert!(!output.text.contains("cleanup failed"), "{}", output.text);
    assert!(!output.text.contains("supervisor"), "{}", output.text);
}

#[tokio::test]
async fn command_directories_are_explicit_local_and_shared_by_background() {
    let dir = TempDir::new("python-command-cwd");
    let context = files(dir.path());
    let output = cell(&context, r#"
base = pathlib.Path.cwd()
child = base / 'directory with spaces'
child.mkdir()
probe = ['python3', '-c', 'import os; print(os.getcwd())']
assert pathlib.Path(run(probe, cwd=child).stdout.strip()) == child
os.chdir(child)
assert pathlib.Path(run(probe).stdout.strip()) == base
assert pathlib.Path(run(probe, cwd='directory with spaces').stdout.strip()) == child
j = bg_run(['python3', '-c', "from pathlib import Path; Path('result').write_text('done')"], cwd=child)
for invalid in ('', '\0', b'.'):
    for command in (run, bg_run):
        try:
            command(['touch', 'must-not-run'], cwd=invalid)
        except BartError:
            pass
        else:
            raise AssertionError('invalid cwd accepted')
for missing in ('missing-directory', child / 'result'):
    try:
        run(['touch', 'must-not-run'], cwd=missing)
    except BartError:
        pass
    else:
        raise AssertionError('unusable cwd accepted')
assert not (base / 'must-not-run').exists()
assert not (child / 'must-not-run').exists()
"#).await;
    assert!(!output.is_error(), "{}", output.text);
    time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("directory with spaces/result")).unwrap(),
        "done"
    );
    assert!(!dir.path().join("result").exists());
}

#[tokio::test]
async fn command_environment_overrides_are_explicit_local_and_shared_by_background() {
    let dir = TempDir::new("python-command-env");
    let context = session(dir.path(), Confinement::Unrestricted, 4000);
    let output = cell(&context, r#"
os.environ['BART_TEST_EXPLICIT_ENV_93C1'] = 'python-only'
probe = ['python3', '-c', "import os;print(os.environ.get('BART_TEST_EXPLICIT_ENV_93C1','absent'))"]
assert run(probe).stdout.strip() == 'absent'
assert run(probe, env={'BART_TEST_EXPLICIT_ENV_93C1': 'first'}).stdout.strip() == 'first'
assert run(probe, env=os.environ).stdout.strip() == 'python-only'
assert run(probe).stdout.strip() == 'absent'
j = bg_run(['python3', '-c', "import os;open('env-result','w').write(os.environ['BART_TEST_EXPLICIT_ENV_93C1'])"], env={'BART_TEST_EXPLICIT_ENV_93C1': 'background'})
"#).await;
    assert!(!output.is_error(), "{}", output.text);
    time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("env-result")).unwrap(),
        "background"
    );
    let invalid = cell(
        &context,
        "run(['touch','must-not-run'], env={'bad=name':'value'})",
    )
    .await;
    assert!(invalid.is_error(), "{}", invalid.text);
    assert!(!dir.path().join("must-not-run").exists());
}

#[tokio::test]
async fn success_statuses_stay_out_of_the_result_without_hiding_failure_or_incomplete_output() {
    let dir = TempDir::new("python-summary-ranges");
    let context = session(dir.path(), Confinement::Unrestricted, 4000);
    let output = cell(
        &context,
        "for _ in range(70): r=run(['true'])\nr=run(['bash','-c','exit 7'], check=False)\nr=run(['true'])",
    )
    .await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(!output.text.contains("] exit 0"), "{}", output.text);
    assert!(
        output.text.contains("cell[1].run[71] exit 7"),
        "{}",
        output.text
    );
    assert!(
        !output.text.contains("process_summaries_incomplete"),
        "{}",
        output.text
    );
    let output = cell(
        &context,
        "r=run(['true']); r=run(['bash','-c','echo prefix; sleep 30 &']); job[r.retained_job].kill(); r=run(['true'])",
    )
    .await;
    assert!(output.text.contains("stdout incomplete"), "{}", output.text);
    assert!(!output.text.contains("\"processes\""), "{}", output.text);
}

#[tokio::test]
async fn user_packages_can_appear_between_cells_without_pth_execution_or_stdlib_shadowing() {
    let dir = TempDir::new("python-user-packages");
    let context = session(dir.path(), Confinement::Unrestricted, 2000);
    let output = cell(&context, r#"
paths = write.__globals__['_import_paths'](['/stdlib', '/global/site-packages'], '/project', '/project:/extra', '/user/site-packages')
assert paths == ['/stdlib', '/project', '/extra', '/user/site-packages', '/global/site-packages'], paths
site_dir = pathlib.Path('new-user-site').resolve()
sys.path[:] = write.__globals__['_import_paths'](sys.path, os.getcwd(), '', str(site_dir))
try:
    import bart_new_user_module_93c1
except ModuleNotFoundError:
    pass
else:
    raise AssertionError('fixture already exists')
run([sys.executable, '-c', "import pathlib,sys;p=pathlib.Path(sys.argv[1]);p.mkdir();(p/'bart_new_user_module_93c1.py').write_text('VALUE = 42')", str(site_dir)])
import bart_new_user_module_93c1
assert bart_new_user_module_93c1.VALUE == 42
(site_dir/'path.pth').write_text("import pathlib;pathlib.Path('pth-ran').touch()")
"#).await;
    assert!(!output.is_error(), "{}", output.text);
    let output = cell(&context, "import bart_new_user_module_93c1; assert bart_new_user_module_93c1.VALUE == 42; assert not pathlib.Path('pth-ran').exists(); print('imported')").await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(output.text.contains("imported"));
}

#[tokio::test]
async fn compaction_inventory_tracks_settled_bindings_without_running_repr() {
    let dir = TempDir::new("python-inventory");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    assert!(context.python_inventory().contains("no live interpreter"));
    let output = cell(&context, "class Meta(type):\n def __eq__(self, other): raise AssertionError('type equality called')\nclass Explosive(metaclass=Meta):\n def __repr__(self): raise AssertionError('repr called')\nthing = Explosive()\nitems = [1,2]\ndef helper(x): return x\nraise ValueError('settled failure')").await;
    assert!(
        output.is_error() && output.text.contains("settled failure"),
        "{}",
        output.text
    );
    let inventory = context.python_inventory();
    assert!(
        inventory.contains("helper") && inventory.contains("items"),
        "{inventory}"
    );
    assert!(inventory.contains("last settled cell"));
    assert!(!inventory.contains("repr called"));
    text(&context, "del helper\nitems = 'new'\nNone", "").await;
    let inventory = context.python_inventory();
    assert!(!inventory.contains("\"name\":\"helper\""), "{inventory}");
    assert!(inventory.contains("\"kind\":\"str\""), "{inventory}");
    {
        let _busy = context.interpreter.slot.lock().await;
        assert!(context.python_inventory().contains("a cell is running"));
    }
    context.reset_python();
    assert!(context.python_inventory().contains("unavailable"));
}

#[tokio::test]
async fn inventory_failure_is_metadata_and_does_not_lose_the_interpreter() {
    let dir = TempDir::new("python-inventory-failure");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    text(&context, "def fail_inventory(): raise RuntimeError('optional metadata failed')\nread.__globals__['_namespace_inventory'] = fail_inventory\nx = 41\nprint('completed')", "completed").await;
    assert!(
        context
            .python_inventory()
            .contains("namespace inspection failed")
    );
    text(&context, "x + 1", "42").await;
}

#[tokio::test]
async fn function_inventory_points_to_defining_cells_through_aliases_and_redefinition() {
    let dir = TempDir::new("python-function-provenance");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let first = cell(&context, "def factory():\n def nested(x, /, *args, scale=2, **kwargs):\n  return x * scale\n return nested\nhelper = factory()\nold_helper = helper").await;
    assert!(!first.is_error(), "{}", first.text);
    let first_cell = first.cell.unwrap();
    let second = cell(&context, "def helper(value):\n return value + 1").await;
    assert!(!second.is_error(), "{}", second.text);
    let inventory = context.python_inventory();
    let value: Value = serde_json::from_str(&inventory[inventory.find('{').unwrap()..]).unwrap();
    let binding = |name: &str| {
        value["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap()
    };
    assert_eq!(
        binding("helper")["source"],
        format!("cell[{}].code", second.cell.unwrap())
    );
    assert_eq!(binding("helper")["line"], 1);
    assert_eq!(
        binding("old_helper")["source"],
        format!("cell[{first_cell}].code")
    );
    assert_eq!(binding("old_helper")["line"], 2);
    assert_eq!(binding("old_helper")["positional_only"], 1);
    assert_eq!(binding("old_helper")["keyword_only"], 1);
    assert_eq!(binding("old_helper")["varargs"], true);
    assert_eq!(binding("old_helper")["kwargs"], true);
    let failed = cell(&context, "old_helper(None)").await;
    assert!(failed.is_error());
    assert!(
        failed.text.contains(&format!("cell[{first_cell}].code")),
        "{}",
        failed.text
    );
    assert!(
        failed
            .text
            .contains(&format!("cell[{}].code", failed.cell.unwrap())),
        "{}",
        failed.text
    );
}

#[tokio::test]
async fn oversized_bare_read_streams_a_prefix_and_saves_the_complete_numbered_view() {
    let dir = TempDir::new("read-prefix-artifact");
    let content = format!("{}LAST_SENTINEL\n", "αβγ\n".repeat(5000));
    fs::write(dir.path().join("many.txt"), &content).unwrap();
    let context = session(dir.path(), Confinement::Unrestricted, 32 * 1024);
    let output = cell(&context, "read('many.txt')").await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(
        output
            .text
            .starts_with("[Bart read: many.txt:1-5001]\n     1\tαβγ"),
        "{}",
        output.text
    );
    assert!(!output.text.contains("LAST_SENTINEL"));
    assert!(!output.notes.is_empty());
    let saved = output.saved.unwrap();
    let full = fs::read_to_string(saved.path).unwrap();
    let end = full[..32768].rfind('\n').unwrap() + 1;
    assert_eq!(&full[..end], &output.text[..end]);
    assert!(output.text[end..].starts_with("[Bart output:"));
    assert!(output.notes.contains("rest: cell[1].remaining()"));
    assert!(full.ends_with("  5001\tLAST_SENTINEL\n"));
    text(&context, "data = read('many.txt')\nassert data.endswith('LAST_SENTINEL\\n')\nassert len(data.splitlines()) == 5001\nprint('raw intact')", "raw intact").await;
}

#[tokio::test]
async fn cell_traceback_source_uses_python_newlines_and_does_not_leak_cache() {
    let dir = TempDir::new("cell-traceback-source");
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    for newline in ["\n", "\r\n", "\r"] {
        for separator in ["\u{000c}", "\u{000b}", "\u{0085}", "\u{2028}", "\u{2029}"] {
            let code =
                format!("# comment{separator}not the failing expression{newline}1 / 0{newline}");
            let output = cell(&context, &code).await;
            assert!(output.is_error(), "{}", output.text);
            assert!(output.text.contains("].code\", line 2"), "{}", output.text);
            assert!(output.text.contains("    1 / 0"), "{}", output.text);
            assert!(
                !output.text.contains("not the failing expression"),
                "{}",
                output.text
            );
        }
    }
    let has_expression_positions = python_has_expression_positions();
    for (code, excerpt, legacy_excerpt) in [
        (
            "x = 'é'; int('bad'); x = 'unrelated'",
            "    int('bad')\n",
            "    x = 'é'; int('bad'); x = 'unrelated'\n",
        ),
        (
            "x = 1; int(\n    'bad'\n); x = 2",
            "    int(\n        'bad'\n    )\n",
            "    x = 1; int(\n",
        ),
        (
            "def fail():\n    x = 'é'; return 1 / 0\nfail()",
            "    1 / 0\n",
            "    x = 'é'; return 1 / 0\n",
        ),
        (
            "int(\n    '0'\n    '1'\n    '2'\n    '3'\n    '4'\n    '5'\n    'bad'\n)",
            "    int(\n        '0'\n        '1'\n    ...\n        '5'\n        'bad'\n    )\n",
            "    int(\n",
        ),
        (
            "try: int('bad')\nexcept ValueError as error: raise ExceptionGroup('group', [error])",
            "    int('bad')\n",
            "    try: int('bad')\n",
        ),
    ] {
        if !has_expression_positions && code.contains("ExceptionGroup") {
            continue;
        }
        let output = cell(&context, code).await;
        assert!(output.is_error(), "{}", output.text);
        let expected = if has_expression_positions {
            excerpt
        } else {
            legacy_excerpt
        };
        assert!(output.text.contains(expected), "{}", output.text);
        if has_expression_positions {
            for unwanted in ["x =", "return 1 / 0", "^", "    '2'", "    '3'", "    '4'"] {
                assert!(!output.text.contains(unwanted), "{}", output.text);
            }
        }
        if code.contains("ExceptionGroup") {
            assert!(
                output.text.contains("ExceptionGroup: group"),
                "{}",
                output.text
            );
            assert!(
                output
                    .text
                    .contains("During handling of the above exception"),
                "{}",
                output.text
            );
            assert_eq!(
                output.text.matches("int('bad')").count(),
                2,
                "{}",
                output.text
            );
        }
    }
    let foreign = cell(
        &context,
        "exec(compile('\\n' * 10 + '1/0', sys._getframe().f_code.co_filename, 'exec'))",
    )
    .await;
    assert!(
        foreign
            .text
            .ends_with("ZeroDivisionError: division by zero"),
        "{}",
        foreign.text
    );
    let syntax = cell(&context, "if True print('bad')").await;
    assert!(syntax.is_error(), "{}", syntax.text);
    assert!(syntax.text.contains("SyntaxError"), "{}", syntax.text);
    assert!(
        syntax.text.contains("if True print('bad')"),
        "{}",
        syntax.text
    );
    assert!(syntax.text.contains('^'), "{}", syntax.text);
    text(
        &context,
        "import linecache\nassert not any(k.startswith('cell[') for k in linecache.cache)\nprint('cache clean')",
        "cache clean",
    )
    .await;
}

#[tokio::test]
async fn imported_project_and_chained_tracebacks_survive_without_harness_frames() {
    let dir = TempDir::new("project-traceback");
    fs::write(
        dir.path().join("scoring.py"),
        "def score():\n    raise ValueError('project failure')\n",
    )
    .unwrap();
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    let output = cell(&context, "import scoring\ntry: scoring.score()\nexcept ValueError as error: raise RuntimeError('wrapper') from error").await;
    assert!(output.is_error(), "{}", output.text);
    for location in [
        "scoring.py",
        "cell[1].code",
        "The above exception was the direct cause",
        "ValueError: project failure",
        "RuntimeError: wrapper",
    ] {
        assert!(output.text.contains(location), "{}", output.text);
    }
    let expected = if python_has_expression_positions() {
        [
            "    scoring.score()\n",
            "    raise RuntimeError('wrapper') from error\n",
        ]
    } else {
        [
            "    try: scoring.score()\n",
            "    except ValueError as error: raise RuntimeError('wrapper') from error\n",
        ]
    };
    for excerpt in expected {
        assert!(output.text.contains(excerpt), "{}", output.text);
    }
    assert!(!output.text.contains("<bart-harness>"), "{}", output.text);
    text(&context, "6 * 7", "42").await;
}

#[tokio::test]
async fn late_process_failures_survive_match_pressure_and_status_overflow_is_recoverable() {
    let dir = TempDir::new("late-process-status");
    let context = session(dir.path(), Confinement::Unrestricted, 32 * 1024);
    let arguments = json!({"code":"print('padding' * 6000); print('MATCH ' + 'x' * 1900)\nfor _ in range(70): r=run(['false'], check=False)\nrecords=[json.loads(line) for line in cell[1].status.read_text().splitlines()]\nassert len(records)==70 and records[-1]['process']==70 and records[-1]['return_code']==1"});
    let call = crate::history::ToolCall {
        id: "status-overflow".into(),
        name: "python".into(),
        arguments: arguments.into(),
    };
    let mut live = context.live.start("python", "statuses", None);
    let mut output = super::super::run(&call, &context, &live, None).await;
    live.finish(output.status);
    super::super::deliver(&mut output, &context, 0, true).await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(output.text.contains(".run[64] exit 1"), "{}", output.text);
    assert!(
        output
            .text
            .contains("[Bart run: 6 more not shown; all records: cell[1].status]"),
        "{}",
        output.text
    );
    assert!(
        output.text.find(".run[64] exit 1").unwrap()
            < output.text.find("6 more not shown; all records:").unwrap(),
        "{}",
        output.text
    );
    assert!(!output.text.contains("records not saved"));
}

#[tokio::test]
async fn a_status_archive_failure_is_explicit_and_does_not_hide_process_outcomes() {
    let dir = TempDir::new("status-save-failure");
    let context = session(dir.path(), Confinement::Unrestricted, 4000);
    fs::create_dir(context.artifacts().dir().join("status-1.jsonl")).unwrap();
    let output = cell(&context, "r=run(['false'], check=False)").await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(output.text.contains("records not saved"), "{}", output.text);
    assert!(
        output.text.find(".run[1] exit 1").unwrap()
            < output.text.find("records not saved").unwrap(),
        "{}",
        output.text
    );
}

#[tokio::test]
async fn web_search_batches_display_and_validate_queries() {
    let dir = TempDir::new("python-web-batch");
    let (provider, requests) = crate::provider::mock::serve(
        dir.path(),
        vec![
            crate::provider::mock::Reply::text("answer"),
            crate::provider::mock::Reply::text("answer"),
        ],
    )
    .await;
    let context = session(dir.path(), Confinement::Unrestricted, 10_000);
    context.route_web(Some(crate::tool::web::Route {
        client: reqwest::Client::new(),
        store: crate::auth::Store::new(dir.path().join("auth.json")),
        provider,
        model: "m".into(),
        effort: None,
        diagnostics: None,
    }));
    for code in [
        "web_search()",
        "web_search(*['q'] * 9)",
        "web_search('q', 1)",
    ] {
        refused(&context, code).await;
    }
    let output = cell(&context, "web_search('first query', 'second query')").await;
    assert_eq!(output.status, Status::Succeeded, "{}", output.text);
    assert!(
        output.text.contains("[Bart web_search: first query]")
            && output.text.contains("[Bart web_search: second query]"),
        "{}",
        output.text
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn background_launches_report_their_handles_once_even_when_assigned() {
    let dir = TempDir::new("background-launch-view");
    let context = session(dir.path(), Confinement::Unrestricted, 1000);
    for code in [
        "for _ in range(1): bg_run(['true'])",
        "j = bg_run(['true'])",
    ] {
        let output = cell(&context, code).await;
        assert!(!output.is_error(), "{}", output.text);
        assert_eq!(output.jobs.len(), 1);
        assert_eq!(output.text.lines().count(), 1, "{}", output.text);
        assert_eq!(
            output.text,
            format!("[Bart job: job[{}] started]", output.jobs[0])
        );
        context.jobs.settled().await;
        context.jobs.take_notifications();
    }
    let printed = cell(&context, "print(j)").await;
    assert_eq!(printed.text, "[Bart job: job[2] exit 0]");
    let refused = cell(&context, "bg_run(['true'], cwd='no-such-cwd')").await;
    assert!(
        refused.is_error() && !refused.text.contains("started]"),
        "{}",
        refused.text
    );
    assert!(
        refused.text.contains("no-such-cwd") && !refused.text.contains("supervisor"),
        "{}",
        refused.text
    );
}

#[tokio::test]
async fn live_jobs_share_result_accessors_and_retain_complete_captures_across_resume() {
    let dir = TempDir::new("job-result-streams");
    let context = session(dir.path(), Confinement::Unrestricted, 12_000);
    let started = cell(&context, r#"
j = bg_run(['bash', '-c', 'echo first; echo warning >&2; echo READY; while [ ! -e release ]; do sleep .01; done; echo final; exit 7'], notify='^READY$')
"#).await;
    assert!(!started.is_error(), "{}", started.text);
    let id = started.jobs[0];
    time::timeout(Duration::from_secs(5), context.jobs.wake_ready())
        .await
        .unwrap();
    context.jobs.take_notifications();
    let partial = cell(
        &context,
        r#"
assert j.outcome is None and j.return_code is None
for access in [lambda: j.stdout, lambda: j.stderr, lambda: bool(j)]:
    try:
        access()
    except BartError:
        pass
    else:
        raise AssertionError('unfinished result was accepted')
assert j.stdout_file.incomplete is not None
assert b'first' in j.stdout_file.path.read_bytes()
print(j)
"#,
    )
    .await;
    assert!(!partial.is_error(), "{}", partial.text);
    assert!(partial.text.contains("running") && partial.text.contains("first"));
    fs::write(dir.path().join("release"), "").unwrap();
    context.jobs.settled().await;
    let complete = cell(
        &context,
        r#"
assert j.stdout == 'first\nREADY\nfinal\n' and j.stderr == 'warning\n'
assert j.outcome == {'exit': 7} and j.return_code == 7 and not j
assert j.stdout_file.incomplete is None and j.stderr_file.incomplete is None
assert not j.timed_out and j.cleanup_error is None
print(j)
"#,
    )
    .await;
    assert!(!complete.is_error(), "{}", complete.text);
    assert!(complete.text.contains("exit 7") && complete.text.contains("warning"));
    let notes = context.jobs.take_notifications();
    let projected = &notes.last().unwrap().text;
    assert!(projected.contains(&format!("job[{id}]")));
    assert!(!projected.contains(&context.spill_dir.to_string_lossy().to_string()));
    context.stop();
    drop(context);
    let context = session(dir.path(), Confinement::Unrestricted, 12_000);
    let resumed = cell(&context, &format!("j=job[{id}]\nassert j.stdout == 'first\\nREADY\\nfinal\\n'\nassert j.stderr == 'warning\\n'\nassert j.outcome == {{'exit':7}}\nprint(j)")).await;
    assert!(!resumed.is_error(), "{}", resumed.text);
    assert!(resumed.text.contains("exit 7"));
}

#[tokio::test]
async fn background_results_preserve_binary_streams_and_signal_or_cancellation_outcomes() {
    let dir = TempDir::new("job-result-outcomes");
    let context = session(dir.path(), Confinement::Unrestricted, 12_000);
    let started = cell(&context, r#"
j=bg_run([sys.executable, '-c', "import sys,os,signal;sys.stdout.buffer.write(b'x'*20000+bytes([255]));sys.stdout.flush();sys.stderr.write('diagnostic');sys.stderr.flush();os.kill(os.getpid(),signal.SIGTERM)"])
"#).await;
    assert!(!started.is_error(), "{}", started.text);
    context.jobs.settled().await;
    let finished = cell(
        &context,
        r#"
assert j.outcome == {'signal': 15} and j.return_code == -15 and not j
assert j.stderr == 'diagnostic'
assert j.stdout_file.path.read_bytes() == b'x'*20000 + bytes([255])
try:
    j.stdout
except BartError:
    pass
else:
    raise AssertionError('invalid UTF-8 was accepted')
print(j)
"#,
    )
    .await;
    assert!(!finished.is_error(), "{}", finished.text);
    assert!(
        finished.text.contains("[Bart job: job[1] signal 15]"),
        "{}",
        &finished.text[..finished.text.len().min(300)]
    );
    assert!(finished.text.len() < 12_400);
    assert!(finished.text.bytes().filter(|&byte| byte == b'x').count() > 6000);
    assert!(finished.notes.contains("cell[2].remaining()"));
    let stopped = cell(
        &context,
        "k=bg_run(['sleep', '30'])\nk.kill()\nassert k.outcome == 'killed' and not k",
    )
    .await;
    assert!(!stopped.is_error(), "{}", stopped.text);
}

#[test]
fn prompt_description_states_the_selected_background_job_lifetime() {
    use crate::tool::{BackgroundJobs, TurnMode};
    for jobs in [BackgroundJobs::Stop, BackgroundJobs::Keep] {
        let text = super::definition(&super::Surface {
            python: "Python 3",
            escape: false,
            web_search: false,
            mode: TurnMode::Prompt(jobs),
            mcp_servers: &[],
        })
        .description;
        assert_eq!(
            text.contains("stops its jobs"),
            jobs == BackgroundJobs::Stop
        );
        assert_eq!(
            text.contains("survive successful completion"),
            jobs == BackgroundJobs::Keep
        );
        assert!(text.contains("build.wait() instead"));
        assert!(text.contains("def wait(self)"));
    }
}

#[tokio::test]
async fn run_error_shows_head_and_tail_for_oversized_failures() {
    let dir = TempDir::new("run-error-head-tail");
    let context = files(dir.path());
    let failed = cell(
        &context,
        r#"run(['python3', '-c', 'import sys\nprint("HEAD_FAIL: line 1")\nfor i in range(400): print(f"noise {i:03} " + "x" * 40)\nprint("TAIL_SUMMARY: done")\nsys.exit(2)'])"#,
    )
    .await;
    assert!(failed.is_error());
    assert!(failed.text.contains("HEAD_FAIL: line 1"), "{}", failed.text);
    assert!(
        failed.text.contains("TAIL_SUMMARY: done"),
        "{}",
        failed.text
    );
    assert!(!failed.text.contains("first "), "{}", failed.text);
    assert!(
        failed.text.contains("[Bart run: cell[1].run[1] ")
            && failed.text.contains("bytes omitted]"),
        "{}",
        failed.text
    );
    assert!(failed.text.len() < 8192);

    let utf8_boundary = cell(
        &context,
        r#"run(['python3', '-c', 'import sys; sys.stdout.buffer.write("€".encode() * 3000); sys.exit(1)'])"#,
    )
    .await;
    assert!(utf8_boundary.is_error());
    assert!(
        utf8_boundary.text.contains("[Bart run: cell[2].run[1] ")
            && !utf8_boundary.text.contains("\\x"),
        "{}",
        utf8_boundary.text
    );

    let head_at_half = cell(
        &context,
        r#"run(['python3', '-c', 'import sys; sys.stdout.write("a" * 1023 + chr(10) + chr(66) * 2500 + "Z" * 2500); sys.exit(1)'])"#,
    )
    .await;
    assert!(head_at_half.is_error());
    assert!(
        !head_at_half.text.contains("BBBB") && head_at_half.text.contains("bytes omitted]"),
        "{}",
        head_at_half.text
    );

    let head_below_half = cell(
        &context,
        r#"run(['python3', '-c', 'import sys; sys.stdout.write("a" * 1022 + chr(10) + chr(66) * 2500 + "Z" * 2500); sys.exit(1)'])"#,
    )
    .await;
    assert!(head_below_half.is_error());
    assert!(
        head_below_half.text.contains("BBBB"),
        "{}",
        head_below_half.text
    );
}
