#![cfg(target_os = "linux")]

use std::{
    fs,
    io::Write as _,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Run {
    child: Child,
    dir: std::path::PathBuf,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn termination_signals_exit_with_their_status_while_stdin_is_open() {
    for (signal, status) in [
        (rustix::process::Signal::INT, 130),
        (rustix::process::Signal::TERM, 143),
        (rustix::process::Signal::HUP, 129),
    ] {
        let dir = std::env::temp_dir().join(format!(
            "bart-headless-{}-{}",
            std::process::id(),
            signal.as_raw()
        ));
        fs::create_dir_all(dir.join("config/bart")).unwrap();
        fs::write(
            dir.join("config/bart/providers.json"),
            r#"{"local":{"protocol":"chat","url":"http://127.0.0.1:1/v1","key":false}}"#,
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_bart"))
            .args(["--provider", "local", "--model", "fixture", "--json"])
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut run = Run { child, dir };
        let mut stdin = run.child.stdin.take().unwrap();
        stdin.write_all(b"partial next line").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                run.child.try_wait().unwrap().is_none(),
                "exited before the signal"
            );
            let proc_status =
                fs::read_to_string(format!("/proc/{}/status", run.child.id())).unwrap();
            let caught = proc_status
                .lines()
                .find_map(|line| line.strip_prefix("SigCgt:\t"))
                .unwrap();
            if u64::from_str_radix(caught, 16).unwrap() & (1 << (signal.as_raw() - 1)) != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "signal handler was not installed"
            );
            thread::sleep(Duration::from_millis(10));
        }
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(i32::try_from(run.child.id()).unwrap()).unwrap(),
            signal,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(exit) = run.child.try_wait().unwrap() {
                assert_eq!(exit.code(), Some(status), "{signal:?}");
                break;
            }
            assert!(Instant::now() < deadline, "the signal waited for stdin EOF");
            thread::sleep(Duration::from_millis(10));
        }
        drop(stdin);
    }
}

#[test]
fn an_inherited_ignored_hangup_stays_ignored() {
    let output = Command::new("python3")
        .args([
            "-c",
            r#"
import os, pathlib, signal, subprocess, sys, tempfile, time
with tempfile.TemporaryDirectory(prefix="bart-nohup-") as root:
    root = pathlib.Path(root); (root/"config/bart").mkdir(parents=True)
    (root/"config/bart/providers.json").write_text('{"local":{"protocol":"chat","url":"http://127.0.0.1:1/v1","key":false}}')
    env = dict(os.environ, XDG_CONFIG_HOME=str(root/"config"), XDG_STATE_HOME=str(root/"state"))
    child = subprocess.Popen([sys.argv[1], "--provider", "local", "--model", "fixture", "--json"], cwd=root, env=env,
        stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_IGN))
    def caught(number):
        status = pathlib.Path(f"/proc/{child.pid}/status").read_text()
        mask = next(line.split()[1] for line in status.splitlines() if line.startswith("SigCgt:"))
        return int(mask, 16) & (1 << (number - 1)) != 0
    deadline = time.monotonic() + 5
    while not caught(signal.SIGTERM):
        assert time.monotonic() < deadline, "no handlers installed"
        time.sleep(0.01)
    assert not caught(signal.SIGHUP), "an ignored SIGHUP was caught"
    child.send_signal(signal.SIGHUP); time.sleep(0.3)
    assert child.poll() is None, "an ignored SIGHUP ended Bart"
    child.send_signal(signal.SIGTERM)
    assert child.wait(timeout=5) == 143
"#,
            env!("CARGO_BIN_EXE_bart"),
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

#[test]
fn relative_xdg_directories_fall_back_to_home() {
    let dir = std::env::temp_dir().join(format!("bart-xdg-{}", std::process::id()));
    let (home, project) = (dir.join("home"), dir.join("project"));
    fs::create_dir_all(home.join(".config/bart")).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(
        home.join(".config/bart/providers.json"),
        r#"{"local":{"protocol":"chat","url":"http://127.0.0.1:1/v1","key":false}}"#,
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_bart"))
        .args([
            "--provider",
            "local",
            "--model",
            "fixture",
            "--prompt",
            "hi",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", "config")
        .env("XDG_STATE_HOME", "state")
        .current_dir(&project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let run = Run { child, dir };
    let sessions = home.join(".local/state/bart/sessions");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sessions.is_dir() {
        assert!(
            Instant::now() < deadline,
            "no session under the home fallback"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let written: Vec<_> = fs::read_dir(&project)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(written.is_empty(), "{written:?}");
    drop(run);
}

#[test]
fn a_jailed_project_holding_bart_records_through_a_symlink_is_refused_before_any_session() {
    let dir = std::env::temp_dir().join(format!("bart-state-link-{}", std::process::id()));
    let project = dir.join("project");
    fs::create_dir_all(project.join("records")).unwrap();
    fs::create_dir_all(dir.join("config/bart")).unwrap();
    fs::write(
        dir.join("config/bart/providers.json"),
        r#"{"local":{"protocol":"chat","url":"http://127.0.0.1:1/v1","key":false}}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(project.join("records"), dir.join("state")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bart"))
        .args([
            "--jail",
            "--provider",
            "local",
            "--model",
            "fixture",
            "--prompt",
            "hi",
        ])
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .current_dir(&project)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let created = project.join("records/bart").exists();
    fs::remove_dir_all(&dir).unwrap();
    assert!(
        !output.status.success() && stderr.contains("session records"),
        "{stderr}"
    );
    assert!(!created);
}

#[test]
fn stdout_holds_only_kept_responses_and_a_closed_stdout_ends_the_run() {
    let output = Command::new("python3")
        .args([
            "-c",
            r#"
import http.server, json, os, pathlib, subprocess, sys, tempfile, threading
owned = tempfile.TemporaryDirectory(prefix="bart-headless-stdout-"); root = pathlib.Path(owned.name)
requests = []
class Server(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'{"data":[]}')
    def do_POST(self):
        messages = json.loads(self.rfile.read(int(self.headers["Content-Length"])))["messages"]
        requests.append(messages)
        def chunk(delta, **extra):
            return ("data: " + json.dumps({"choices": [{"index": 0, "delta": delta, **extra}]}) + "\n\n").encode()
        self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.end_headers()
        if messages[-1].get("content") == "plain":
            self.wfile.write(chunk({"content": "PLAIN"}) + chunk({}, finish_reason="stop") + b"data: [DONE]\n\n"); return
        if len(requests) == 1:
            self.wfile.write(chunk({"content": "DISCARDED"})); self.wfile.flush()
            self.close_connection = True; return
        if not any(m.get("role") == "tool" for m in messages):
            call = {"index": 0, "id": "c", "type": "function", "function": {"name": "python", "arguments": json.dumps({"code": "1"})}}
            self.wfile.write(chunk({"content": "BEFORE THE TOOL"}) + chunk({"tool_calls": [call]}) + chunk({}, finish_reason="tool_calls"))
        else:
            self.wfile.write(chunk({"content": "FINAL\n"}) + chunk({}, finish_reason="stop"))
        self.wfile.write(b"data: [DONE]\n\n")
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
threading.Thread(target=server.serve_forever, daemon=True).start()
(root/"config/bart").mkdir(parents=True)
(root/"config/bart/providers.json").write_text(json.dumps({"local": {"protocol": "chat", "url": f"http://127.0.0.1:{server.server_port}/v1", "key": False}}))
(root/"config/bart/settings.json").write_text('{"models":{"local":{"fixture":{"context_window":100000}}}}')
env = dict(os.environ, XDG_CONFIG_HOME=str(root/"config"), XDG_STATE_HOME=str(root/"state"))
bart = [sys.argv[1], "--provider", "local", "--model", "fixture", "--prompt", "go"]
run = subprocess.run(bart, cwd=root, env=env, capture_output=True, timeout=60)
assert run.returncode == 0 and run.stdout == b"BEFORE THE TOOL\nFINAL\n", (run.returncode, run.stdout, run.stderr)
assert b"partial response is discarded" in run.stderr, run.stderr
requests.clear()
reader, writer = os.pipe(); os.close(reader)
broken = subprocess.run(bart, cwd=root, env=env, stdout=writer, stderr=subprocess.PIPE, timeout=60)
os.close(writer)
assert broken.returncode == 1 and b"cannot write to stdout" in broken.stderr, (broken.returncode, broken.stderr)
assert len(requests) == 2, f"kept asking the model after stdout closed: {len(requests)} requests"
reader, writer = os.pipe(); os.close(reader)
waiting = subprocess.Popen(bart[:-2] + ["--json"], cwd=root, env=env, stdin=subprocess.PIPE, stdout=writer, stderr=subprocess.PIPE)
os.close(writer); waiting.stdin.write(b'{"type":"user","text":"plain"}\n'); waiting.stdin.flush()
try: status = waiting.wait(timeout=30)
except subprocess.TimeoutExpired: waiting.kill(); raise AssertionError("waited for more stdin after stdout closed")
assert status == 1, (status, waiting.stderr.read())
server.shutdown(); owned.cleanup()
"#,
            env!("CARGO_BIN_EXE_bart"),
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

#[test]
fn a_json_session_reports_turns_and_jobs_and_eof_stops_outstanding_jobs() {
    let output=Command::new("python3").args(["-c",r#"
import http.server, json, os, pathlib, signal, subprocess, sys, tempfile, threading
owned=tempfile.TemporaryDirectory(prefix="bart-headless-lifecycle-")
root=pathlib.Path(owned.name); ready=threading.Event(); requests=[]
class Server(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_GET(self):
        body=b'{"data":[]}'
        self.send_response(200); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(request)
        messages=request['messages']
        users=[m['content'] for m in messages if m['role']=='user']
        if users[-1]=='fail':
            self.send_response(400); self.end_headers(); self.wfile.write(b'{"error":{"message":"fixture refusal"}}'); return
        if not any(m.get('role')=='tool' for m in messages):
            code='bg_run(["bash", "-c", "while [ ! -e release ]; do sleep .01; done; printf final-%s job; touch job-done"])'
            delta={'tool_calls':[{'index':0,'id':'job','type':'function','function':{'name':'python','arguments':json.dumps({'code':code})}}]}
            reason='tool_calls'
        else:
            delta={'content':'later input answered'}; reason='stop'; ready.set()
        body=''.join('data: '+json.dumps({'choices':[{'index':0,'delta':d,**extra}]})+'\n\n' for d,extra in [(delta,{}),({}, {'finish_reason':reason})])+'data: [DONE]\n\n'
        self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.end_headers(); self.wfile.write(body.encode())
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Server)
threading.Thread(target=server.serve_forever,daemon=True).start()
config=root/'config'/'bart'; config.mkdir(parents=True)
(config/'providers.json').write_text(json.dumps({'local':{'protocol':'chat','url':f'http://127.0.0.1:{server.server_port}/v1','key':False}}))
(config/'settings.json').write_text('{"models":{"local":{"fixture":{"context_window":100000}}}}')
env=dict(os.environ,XDG_CONFIG_HOME=str(root/'config'),XDG_STATE_HOME=str(root/'state'))
child=subprocess.Popen([sys.argv[1],'--provider','local','--model','fixture','--json'],cwd=root,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
deadline=threading.Timer(30, child.kill); deadline.start()
try:
    events=[]
    def turn(message):
        child.stdin.write((json.dumps(message)+'\n').encode()); child.stdin.flush()
        while True:
            event=json.loads(child.stdout.readline()); events.append(event)
            if event['type']=='turn_done': return event
    assert turn({'type':'user','text':'fail'})['status']=='failed'
    done=turn({'type':'user','text':'run'})
    assert done['status']=='completed' and done['message']=='later input answered' and done['jobs_running']==[1],done
    kinds=[event['type'] for event in events]
    assert kinds[0]=='session' and events[0]['id'] and 'tool_started' in kinds and 'tool_finished' in kinds and 'usage' not in kinds[:1],kinds
    child.stdin.close(); child.stdin=None
    assert child.wait(timeout=10)==0
    assert not (root/'job-done').exists(), 'job outlived the session'
finally:
    deadline.cancel()
    (root/'release').touch()
    try: child.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        child.send_signal(signal.SIGINT)
        try: child.communicate(timeout=5)
        except subprocess.TimeoutExpired: child.kill(); child.communicate(timeout=5)
    server.shutdown(); owned.cleanup()
"#,env!("CARGO_BIN_EXE_bart")]).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn skill_commands_attach_only_at_the_prefix_and_errors_send_no_request() {
    let output=Command::new("python3").args(["-c",r#"
import http.server,json,os,pathlib,subprocess,sys,tempfile,threading
with tempfile.TemporaryDirectory(prefix='bart-skill-cli-') as temp:
 root=pathlib.Path(temp); config=root/'config/bart';config.mkdir(parents=True)
 folder=root/'.agents/skills/review';folder.mkdir(parents=True)
 body='---\nname: review\ndescription: A test workflow\n---\nSKILL_BODY_ONLY\n'
 (folder/'SKILL.md').write_text(body)
 seen=[]
 class Server(http.server.BaseHTTPRequestHandler):
  def log_message(self,*args):pass
  def do_GET(self):
   self.send_response(200);self.end_headers();self.wfile.write(b'{"data":[]}')
  def do_POST(self):
   seen.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
   data='data: '+json.dumps({'choices':[{'index':0,'delta':{'content':'done'}}]})+'\n\ndata: '+json.dumps({'choices':[{'index':0,'delta':{},'finish_reason':'stop'}]})+'\n\ndata: [DONE]\n\n'
   self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers();self.wfile.write(data.encode())
 server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Server)
 threading.Thread(target=server.serve_forever,daemon=True).start()
 (config/'providers.json').write_text(json.dumps({'local':{'protocol':'chat','url':f'http://127.0.0.1:{server.server_port}/v1','key':False}}))
 (config/'settings.json').write_text('{"models":{"local":{"fixture":{"context_window":100000}}}}')
 env=dict(os.environ,HOME=str(root),XDG_CONFIG_HOME=str(root/'config'),XDG_STATE_HOME=str(root/'state'))
 try:
  def run(prompt):
   return subprocess.run([sys.argv[1],'--provider','local','--model','fixture','--prompt',prompt],env=env,cwd=root,capture_output=True,text=True,timeout=15)
  first=run('/review inspect');assert first.returncode==0,(first.stdout,first.stderr)
  request=seen[-1];assert any(m['content'].startswith('[Bart skill: review; ') and m['content'].endswith(body) for m in request['messages'])
  assert 'SKILL_BODY_ONLY' not in request['messages'][0]['content']
  assert 'Skill: review' in first.stderr and 'SKILL_BODY_ONLY' not in first.stderr
  for prompt in ['/missing task','/../review task','/ review task','/tmp/x has the results','Discuss /review without invoking it']:
   count=len(seen)
   plain=run(prompt);assert plain.returncode==0 and len(seen)==count+1,(prompt,plain.stdout,plain.stderr)
   assert any(m['role']=='user' and m['content']==prompt for m in seen[-1]['messages'])
   assert not any('SKILL_BODY_ONLY' in str(m['content']) for m in seen[-1]['messages'])
  count=len(seen)
  for invalid in [b'',b'\xff',b'x'*65537]:
   (folder/'SKILL.md').write_bytes(invalid)
   failed=run('/review inspect');assert failed.returncode!=0 and len(seen)==count,(failed.stdout,failed.stderr)
 finally:server.shutdown()
"# ,env!("CARGO_BIN_EXE_bart")]).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn idle_mcp_startup_failures_are_reported_once_without_secrets_or_terminal_controls() {
    let output = Command::new("python3").args(["-c", r#"
import json, os, pathlib, subprocess, sys, tempfile, threading
with tempfile.TemporaryDirectory(prefix="bart-mcp-diagnostic-") as root:
    root = pathlib.Path(root); config = root / "config/bart"; config.mkdir(parents=True)
    (config/"providers.json").write_text(json.dumps({"local":{"protocol":"chat","url":"http://127.0.0.1:1/v1","key":False}}))
    (config/"settings.json").write_text('{"models":{"local":{"fixture":{"context_window":100000}}}}')
    secret = "synthetic-mcp-headless-secret"
    code = "import os,sys; print('x'*10000 + os.environ['API_TOKEN'] + '\\x1b[31m broken cause', file=sys.stderr, flush=True); sys.exit(3)"
    (config/"mcp.json").write_text(json.dumps({"mcpServers":{"broken":{"command":"python3","args":["-c",code],"env":{"API_TOKEN":secret}}}}))
    child = subprocess.Popen([sys.argv[1],"--provider","local","--model","fixture","--json"], cwd=root,
        env=dict(os.environ, XDG_CONFIG_HOME=str(root/"config"), XDG_STATE_HOME=str(root/"state")),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    seen = bytearray(); reported = threading.Event()
    def collect():
        for line in child.stderr:
            seen.extend(line)
            if b"MCP broken:" in seen: reported.set()
    reader = threading.Thread(target=collect); reader.start()
    try:
        assert reported.wait(10), "failure was not reported while idle"
        child.stdin.close()
        assert child.wait(timeout=10) == 0
        reader.join(timeout=5)
        assert seen.count(b"MCP broken:") == 1, seen
        assert b"broken cause" in seen and secret.encode() not in seen and b"\x1b" not in seen, seen
        assert len(seen) < 5000, len(seen)
    finally:
        if child.poll() is None: child.kill(); child.wait()
        reader.join(timeout=5)
"#, env!("CARGO_BIN_EXE_bart")]).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inherited_jail_rejects_background_survival_before_opening_a_session() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("config/bart")).unwrap();
    fs::write(
        dir.path().join("config/bart/settings.json"),
        r#"{"permissions":{"sandbox":{"enabled":true}}}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bart"))
        .args(["--prompt", "test", "--keep-background-jobs"])
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--keep-background-jobs cannot be used in a jail")
    );
    assert!(!dir.path().join("state/bart/sessions").exists());
}
