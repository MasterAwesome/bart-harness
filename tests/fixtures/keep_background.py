import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

binary = str(Path(sys.argv[1]).resolve())
for mode in ['keep', 'cancel']:
    with tempfile.TemporaryDirectory(prefix='bart-lifetime-') as tmp:
        root = Path(tmp)
        requests = []
        second = threading.Event()
        unblock = threading.Event()
        child_code = "import os,time,pathlib; pathlib.Path('pid').write_text(str(os.getpid()))\nwhile not pathlib.Path('release').exists(): time.sleep(.01)\nprint('AFTER-EXIT',flush=True)\n__import__('sys').stdout.write('x' * (256 * 1024))\n__import__('sys').stdout.flush()\nprint('AFTER-ERR',file=__import__('sys').stderr,flush=True)\npathlib.Path('finished').write_text('done')"
        code = (
            "import socket\ns = socket.socket(socket.AF_UNIX)\ns.bind('python.sock')\ns.listen()\n"
            "b = bg_run(['python3','-c'," + repr(child_code) + "])\n"
            "f = bg_run(['bash','-c','echo finite'])\nf.wait()\nassert f.return_code == 0\n"
            "import pathlib,time\nwhile not pathlib.Path('pid').exists(): time.sleep(.01)"
        )
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"data":[]}')
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                if len(requests) == 2:
                    second.set()
                    if mode == 'cancel':
                        unblock.wait(15)
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()
                if len(requests) == 1:
                    delta = {'tool_calls': [{'index': 0, 'id': 'work', 'type': 'function', 'function': {'name': 'python', 'arguments': json.dumps({'code': code})}}]}
                    reason = 'tool_calls'
                else:
                    delta, reason = {'content': 'done'}, 'stop'
                chunks = [{'choices': [{'index': 0, 'delta': delta}]}, {'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}]}]
                try:
                    for chunk in chunks:
                        self.wfile.write(('data: ' + json.dumps(chunk) + '\n\n').encode())
                    self.wfile.write(b'data: [DONE]\n\n')
                except (BrokenPipeError, ConnectionResetError):
                    pass
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        config = root/'config/bart'
        config.mkdir(parents=True)
        (config/'providers.json').write_text(json.dumps({'mock': {'protocol': 'chat', 'url': f'http://127.0.0.1:{server.server_port}/v1', 'key': False}}))
        (config/'settings.json').write_text('{"models":{"mock":{"fixture":{"context_window":100000}}}}')
        process = subprocess.Popen([binary, '-p', 'test', '--keep-background-jobs', '--provider', 'mock', '--model', 'fixture'], cwd=root, env=dict(os.environ, XDG_CONFIG_HOME=str(root/'config'), XDG_STATE_HOME=str(root/'state')), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            assert second.wait(15), 'second request not reached'
            if mode == 'cancel':
                process.send_signal(signal.SIGTERM)
            out, err = process.communicate(timeout=15)
            assert process.returncode == (143 if mode == 'cancel' else 0), (out, err)
            assert len(requests) == 2
            assert 'def wait(self)' in json.dumps(requests[0]['tools'])
            client = socket.socket(socket.AF_UNIX)
            try:
                client.connect(str(root/'python.sock'))
                raise AssertionError('Python interpreter socket survived')
            except ConnectionRefusedError:
                pass
            finally:
                client.close()
            pid = int((root/'pid').read_text())
            def alive():
                state = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True).stdout.strip()
                return bool(state) and not state.startswith('Z')
            assert alive() == (mode == 'keep'), mode
            (root/'release').touch()
            deadline = time.monotonic() + 5
            while alive() and time.monotonic() < deadline:
                time.sleep(.01)
            assert not alive(), 'service did not finish'
            if mode == 'keep':
                assert (root/'finished').read_text() == 'done', 'output writes failed after exit'
                streams = [p for p in (root/'state').rglob('*') if p.suffix in ['.stdout', '.stderr']]
                assert streams, 'no raw capture files found'
                assert all(b'AFTER-' not in p.read_bytes() for p in streams), 'post-handoff output was captured'
        finally:
            (root/'release').touch()
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)
            unblock.set()
            server.shutdown()
print('PASS: actual CLI exit preserves jobs and discards their output, releases Python, waits finite work; SIGTERM cleans up')
