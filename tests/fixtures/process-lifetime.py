import http.server
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import threading
import time


def state(pid):
    return subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True).stdout.strip()[:1]


def children(pid):
    rows = subprocess.run(['ps', '-A', '-o', 'pid=,ppid='], capture_output=True, text=True, check=True).stdout.split()
    return [int(child) for child, parent in zip(rows[::2], rows[1::2]) if int(parent) == pid]


detach = "os.setsid(); " if sys.platform == "linux" else ""
with tempfile.TemporaryDirectory(prefix="bart-owner-death-") as directory:
    root = pathlib.Path(directory)
    ready = threading.Event()
    daemon = f"import os,time; p=os.fork(); os._exit(0) if p else None; {detach}p=os.fork(); os._exit(0) if p else None; open('daemon-pid','w').write(str(os.getpid())); time.sleep(60)"
    command = f"python3 -c {json.dumps(daemon)} >/dev/null 2>&1 & until [ -s daemon-pid ]; do sleep .01; done; exit 5"
    code = f"""for i in range(64):
    r = run(['bash','-c',f'printf fast; exit {{i % 8}}'])
    assert r.return_code == i % 8 and r.stdout.strip() == 'fast', (r.return_code, r.stdout)
r = run(['bash','-c',{command!r}])
assert r.return_code == 5 and r.retained_job is not None, (r.return_code, r.retained_job)
import subprocess, sys
p = subprocess.Popen([sys.executable, '-c', "import os,time; open('python-pid','w').write(str(os.getpid())); time.sleep(60)"], start_new_session=sys.platform == 'linux', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
while not pathlib.Path('python-pid').exists(): time.sleep(.01)
print('OWNERS-READY')"""

    class Server(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"data":[]}')

        def do_POST(self):
            messages = json.loads(self.rfile.read(int(self.headers["Content-Length"])))['messages']
            tools = [message for message in messages if message['role'] == 'tool']
            if tools:
                assert 'OWNERS-READY' in json.dumps(tools), tools
                ready.set()
                delta = {'content': 'ready'}
                reason = 'stop'
            else:
                delta = {'tool_calls': [{'index': 0, 'id': 'owned', 'type': 'function', 'function': {'name': 'python', 'arguments': json.dumps({'code': code})}}]}
                reason = 'tool_calls'
            body = ''.join('data: ' + json.dumps({'choices': [{'index': 0, 'delta': part, **extra}]}) + '\n\n'
                           for part, extra in [(delta, {}), ({}, {'finish_reason': reason})]) + 'data: [DONE]\n\n'
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            self.wfile.write(body.encode())

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = root / 'config' / 'bart'
    config.mkdir(parents=True)
    (config / 'providers.json').write_text(json.dumps({'local': {'protocol': 'chat', 'url': f'http://127.0.0.1:{server.server_port}/v1', 'key': False}}))
    (config / 'settings.json').write_text(json.dumps({'models': {'local': {'fixture': {'context_window': 100000}}}}))
    env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_STATE_HOME=str(root / 'state'))
    bart = subprocess.Popen([sys.argv[1], '--provider', 'local', '--model', 'fixture', '--json'], cwd=root, env=env,
                            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
    foreign = subprocess.Popen(['sleep', '60'])
    try:
        bart.stdin.write(b'{"type":"user","text":"run"}\n')
        bart.stdin.flush()
        assert ready.wait(20), 'Bart did not finish the ownership cell'
        assert bart.poll() is None, 'Bart exited with a running adopted job'
        supervisors = children(bart.pid)
        assert len(supervisors) == 2, f'fast sh calls leaked supervisors: {supervisors}'
        for pid in supervisors:
            assert state(pid) != 'Z', 'unreaped supervisor'
        descendants = [int((root / name).read_text()) for name in ['daemon-pid', 'python-pid']]
        if sys.argv[2] == 'group-term':
            os.killpg(bart.pid, signal.SIGTERM)
            assert bart.wait(timeout=5) == 143
        else:
            bart.send_signal(signal.SIGKILL)
            assert bart.wait(timeout=5) == -signal.SIGKILL
        for pid in supervisors + descendants:
            deadline = time.monotonic() + 7
            while state(pid) not in ('', 'Z'):
                assert time.monotonic() < deadline, f'process {pid} survived Bart termination'
                time.sleep(.01)
        assert foreign.poll() is None, 'cleanup killed a process Bart did not start'
    finally:
        if bart.poll() is None:
            bart.kill()
        bart.wait(timeout=10)
        foreign.kill()
        foreign.wait(timeout=5)
        server.shutdown()
