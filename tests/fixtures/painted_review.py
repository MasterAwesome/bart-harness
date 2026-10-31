import hashlib
import http.server
import json
import os
import pathlib
import pty
import re
import select
import fcntl
import termios
import struct
import signal
import subprocess
import sys
import tempfile
import threading
import time

binary = pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="bart-painted-review-") as temporary:
    root = pathlib.Path(temporary).resolve()
    cwd = root / "project"
    config = root / "config/bart"
    cwd.mkdir()
    config.mkdir(parents=True)
    marker = cwd / "approved"

    class Server(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            messages = json.loads(self.rfile.read(int(self.headers["Content-Length"])))['messages']
            if any(m.get('role') == 'tool' for m in messages):
                delta, reason = {'content': 'APPROVAL_FINISHED'}, 'stop'
            else:
                code = 'from pathlib import Path; Path("approved").touch()'
                delta = {'tool_calls': [{'index': 0, 'id': 'paint', 'type': 'function',
                         'function': {'name': 'python', 'arguments': json.dumps({'code': code})}}]}
                reason = 'tool_calls'
            body = ''.join('data: ' + json.dumps({'choices': [{'index': 0, 'delta': d, **extra}]}) + '\n\n'
                           for d, extra in [(delta, {}), ({}, {'finish_reason': reason})]) + 'data: [DONE]\n\n'
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            self.wfile.write(body.encode())

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Server)
    (config / 'providers.json').write_text(json.dumps({'fixture': {'protocol': 'chat',
        'url': f'http://127.0.0.1:{server.server_port}/v1', 'key': False}}))
    (config / 'settings.json').write_text(json.dumps({'models': {'fixture': {'fixture': {'context_window': 100000}}}}))
    project = cwd.name + '-' + hashlib.sha256(str(cwd).encode()).hexdigest()[:8]
    victim = root / 'state/bart/sessions' / project / '1-victim'
    victim.mkdir(parents=True)
    entries = [
        {'event': 'session', 'data': {'version': 1, 'cwd': str(cwd), 'provider': 'fixture',
         'model': 'fixture', 'effort': None, 'project': ''}, 'time_ms': 1},
        {'event': 'messages', 'data': [{'role': 'user', 'content': 'DELETE_TARGET'}], 'time_ms': 2},
    ]
    (victim / 'session.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in entries))
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.environ.update(HOME=str(root), XDG_CONFIG_HOME=str(root / 'config'),
                          XDG_STATE_HOME=str(root / 'state'), TERM='xterm-256color')
        os.execv(binary, [str(binary), '--provider', 'fixture', '--model', 'fixture', '--approval', 'manual'])
    threading.Thread(target=server.serve_forever, daemon=True).start()
    output = b''

    def pump():
        global output
        if select.select([master], [], [], .02)[0]:
            try:
                part = os.read(master, 65536)
            except OSError as error:
                raise AssertionError(output.decode(errors='replace')) from error
            output += part
            if b'\x1b[6n' in part:
                os.write(master, b'\x1b[1;1R')

    def until(predicate, label):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            pump()
            if predicate():
                return
        raise AssertionError(label + '\n' + output.decode(errors='replace'))

    def shown(text):
        until(lambda: re.sub(rb'\s', b'', text.encode()) in re.sub(rb'\s|\x1b\[[0-?]*[ -/]*[@-~]', b'', output), text)

    def key(text):
        os.write(master, text.encode())

    def command(text):
        key(text + '\r')

    def resize(width, height):
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', height, width, 0, 0))

    def resize_with_queued_action(width, height, action='\r'):
        global output
        os.kill(pid, signal.SIGSTOP)
        stopped, status = os.waitpid(pid, os.WUNTRACED)
        assert stopped == pid and os.WIFSTOPPED(status)
        output = b''
        resize(width, height)
        key(action)
        os.kill(pid, signal.SIGCONT)

    try:
        resize(100, 30)
        shown('ready')
        output = b''
        command('/delete')
        shown('DELETE_TARGET')
        key('DELETE_TARGET\r')
        shown('permanently deleted')
        key('\x1b[I')
        shown('Enter delete')
        resize_with_queued_action(80, 29)
        shown('Enter delete')
        assert victim.exists(), 'stale delete acted'
        key('\r')
        until(lambda: not victim.exists(), 'delete after repaint')
        key('\x1b')
        shown('ready')
        output = b''
        command('run the tool')
        shown('approval required')
        key('\x1b[I')
        shown('a approve')
        resize_with_queued_action(90, 28, 'a')
        shown('a approve')
        assert not marker.exists(), 'stale approval acted'
        key('a')
        until(marker.exists, 'approval after repaint')
        print('Confirm and approval: stale resize action blocked; repainted action accepted')
    finally:
        os.kill(pid, signal.SIGCONT)
        os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + 15
        while not os.waitpid(pid, os.WNOHANG)[0]:
            if time.monotonic() > deadline:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
                break
            if select.select([master], [], [], .02)[0]:
                try:
                    os.read(master, 65536)
                except OSError:
                    time.sleep(.02)
        os.close(master)
        server.shutdown()
        server.server_close()
