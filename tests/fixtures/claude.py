#!/usr/bin/env python3
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def send(value):
    print(json.dumps(value), flush=True)


def record(event, **fields):
    with open(os.environ["FIXTURE_LOG"], "a") as log:
        log.write(json.dumps(dict(event=event, pid=os.getpid(), **fields)) + "\n")


def callback(index, code=None):
    send({"type": "control_request", "request_id": f"callback-{index}", "request": {
        "subtype": "mcp_message", "server_name": "bart", "message": {
            "jsonrpc": "2.0", "id": index, "method": "tools/call", "params": {
                "name": "python", "arguments": {"code": code or CODES[index]},
                "_meta": {"claudecode/toolUseId": f"native-{index}", "progressToken": 7}}}}})


def response(text=None, codes=None, before_stop=lambda: None):
    codes = codes or CODES
    send({"type": "stream_event", "event": {"type": "message_start", "message": {
        "id": "message", "model": "fixture", "usage": {"input_tokens": 40000 if text == "NEXT" else 10,
        "cache_read_input_tokens": 30, "cache_creation_input_tokens": 4, "output_tokens": 0}}}})
    for index in ([1] if text else sorted(codes)):
        block = {"type": "text", "text": ""} if text else {
            "type": "tool_use", "id": f"native-{index}", "name": "mcp__bart__python", "input": {}}
        send({"type": "stream_event", "event": {"type": "content_block_start", "index": index - 1, "content_block": block}})
        delta = {"type": "text_delta", "text": text} if text else {
            "type": "input_json_delta", "partial_json": json.dumps({"code": codes[index]})}
        send({"type": "stream_event", "event": {"type": "content_block_delta", "index": index - 1, "delta": delta}})
        send({"type": "stream_event", "event": {"type": "content_block_stop", "index": index - 1}})
    send({"type": "stream_event", "event": {"type": "message_delta", "delta": {
        "stop_reason": "end_turn" if text else "tool_use"}, "usage": {"output_tokens": 6}}})
    before_stop()
    send({"type": "stream_event", "event": {"type": "message_stop"}})
    if text:
        send({"type": "result", "subtype": "success", "is_error": False, "total_cost_usd": 0.01})
    elif codes is CODES:
        callback(1)


LATE = False


def marker(name):
    return f'from pathlib import Path\nPath("{name}").write_text("executed")'


def unsafe(mode):
    if mode == "cancelled":
        def cancel():
            callback(1, marker(mode))
            send({"type": "control_cancel_request", "request_id": "callback-1"})
        response(codes={1: marker(mode)}, before_stop=cancel)
    elif mode == "mismatched":
        response(codes={1: marker(mode)})
        callback(1, "print(999)")
    elif mode == "missing":
        response(codes={1: marker(mode)})
        send({"type": "result", "subtype": "success", "is_error": False})
    else:
        global LATE
        LATE = True
        response(codes={1: marker("late-1"), 2: marker("late-2")})
        callback(1, marker("late-1"))


PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP4/x8AAwAB/2+Bq7YAAAAASUVORK5CYII="

CODES = {index: f'from pathlib import Path\np = Path("executions")\np.write_text((p.read_text() if p.exists() else "") + "{letter}")\nprint({answer})' for index, letter, answer in [(1, "a", 42), (2, "b", 56)]}
CODES[1] += '\nbackground = bg_run(["true"])'
CODES[2] += f'\nwhile background.outcome is None:\n    time.sleep(0.01)\nimport base64\nPath("pixel.png").write_bytes(base64.b64decode("{PIXEL}"))\ndisplay("pixel.png")'


def fixture():
    assert os.environ["DISABLE_AUTO_COMPACT"] == "1"
    assert "--no-session-persistence" in sys.argv and sys.argv[sys.argv.index("--tools") + 1] == ""
    assert "--append-system-prompt-file" not in sys.argv and "--system-prompt-file" not in sys.argv
    configured = "bart" in json.loads(sys.argv[sys.argv.index("--mcp-config") + 1])["mcpServers"]
    summary = False
    for line in sys.stdin:
        frame = json.loads(line)
        if frame["type"] == "control_request":
            subtype = frame["request"]["subtype"]
            if subtype == "initialize":
                prompt = "".join(frame["request"].get("systemPrompt", []))
                summary = "Tool definitions are reference only" in prompt
                record("start", configured=configured, summary=summary)
                value = {"claude_code_version": "2.1.316", "models": [{"value": "fixture", "supportedEffortLevels": ["medium"]}]}
            elif subtype == "get_context_usage":
                assert frame["request"].get("detail") == "summary"
                value = {"rawMaxTokens": 100000, "isAutoCompactEnabled": False}
            elif subtype == "interrupt":
                record("interrupted")
                value = {}
            else:
                raise AssertionError(subtype)
            send({"type": "control_response", "response": {"subtype": "success", "request_id": frame["request_id"], "response": value}})
        elif frame["type"] == "user":
            content = frame["message"]["content"]
            assert all(block["type"] in ("text", "image") for block in content)
            text = "\n".join(block.get("text", "") for block in content)
            record("input", summary=summary, text=text)
            if text.strip() == "limited":
                if not any(e["event"] == "limited" for e in events(Path(os.environ["FIXTURE_LOG"]))):
                    record("limited")
                    send({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "resetsAt": int(time.time()) + 2,
                          "rateLimitType": "five_hour", "unifiedWindows": {"five_hour": {"utilization": 1.0}}}})
                    send({"type": "result", "subtype": "success", "is_error": True, "api_error_status": 429,
                          "result": "You've hit your limit"})
                else:
                    response("RESUMED")
            elif text.strip().startswith("unsafe-"):
                unsafe(text.strip().removeprefix("unsafe-"))
            elif "wait-for-cancel" in text:
                child = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(1000)"])
                record("waiting", child=child.pid)
            elif summary:
                response("The user requested a calculation. Python ran once and returned 42. Continue from this checkpoint.")
            elif text.strip() == "go":
                response()
            else:
                response("AFTER" if "after-checkpoint" in text else "NEXT")
        elif frame["type"] == "control_response":
            result = frame["response"]["response"]["mcp_response"]
            index = result["id"]
            if LATE:
                callback(2, "print(999)")
                continue
            assert str(42 if index == 1 else 56) in result["result"]["content"][0]["text"]
            images = [block for block in result["result"]["content"] if block["type"] == "image"]
            assert images == ([{"type": "image", "data": PIXEL, "mimeType": "image/png"}] if index == 2 else []), images
            notes = [block for block in result["result"]["content"] if '"bart_event":"background_completion"' in block.get("text", "")]
            assert len(notes) == (index == 2), result
            record("released", call=index)
            if index == 1:
                callback(2)
            else:
                response("DONE")
        else:
            raise AssertionError(frame["type"])


def events(path):
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def running(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    if sys.platform.startswith("linux"):
        try:
            return not Path(f"/proc/{pid}/stat").read_text().split(") ")[1].startswith("Z")
        except (FileNotFoundError, ProcessLookupError):
            return False
    return True


def check(binary):
    with tempfile.TemporaryDirectory(prefix="bart-claude-boundary-") as root:
        root = Path(root)
        executable = root / "claude"
        executable.write_text(Path(__file__).read_text())
        executable.chmod(0o700)
        log = root / "wire.jsonl"
        env = dict(os.environ, BART_CLAUDE_PATH=str(executable), FIXTURE_LOG=str(log),
                   XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"))
        args = [binary, "--temporary", "--provider", "claude", "--model", "fixture", "--effort", "medium"]
        process = subprocess.Popen(args, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=root)
        try:
            output, error = process.communicate("go\nnext\n/compact\nafter-checkpoint\n", timeout=30)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                process.communicate(timeout=5)
        assert process.returncode == 0, error
        assert output.split() == ["DONE", "NEXT", "AFTER"], output
        assert (root / "executions").read_text() == "ab"
        seen = events(log)
        mains = [e for e in seen if e["event"] == "start" and e["configured"] and not e["summary"]]
        assert len(mains) == 2, seen
        first = [e for e in seen if e["pid"] == mains[0]["pid"] and e["event"] == "input"]
        assert len(first) == 2, first
        assert len([e for e in seen if e["event"] == "released"]) == 2
        last = [e for e in seen if e["pid"] == mains[1]["pid"] and e["event"] == "input"][0]
        assert '"bart_origin":"host_context"' in last["text"], last
        assert "~$0.010000" in error, error
        limited = subprocess.run(args + ["--prompt", "limited"], cwd=root, env=env, text=True,
                                 stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        assert limited.returncode == 0 and limited.stdout.split() == ["RESUMED"], limited
        assert "HTTP 429 Too Many Requests: You've hit your limit" in limited.stderr, limited.stderr
        assert "for reset, then continuing" in limited.stderr, limited.stderr
        for mode in ["cancelled", "mismatched", "missing", "late"]:
            persistent = args[:1] + args[2:] if mode == "late" else args
            failed = subprocess.run(persistent + ["--prompt", f"unsafe-{mode}"], cwd=root, env=env,
                                    text=True, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
            assert failed.returncode != 0 and "response failed (claude_cli)" in failed.stderr, failed
            started = failed.stderr.count("\u2192 python(")
            assert started == (mode == "late"), failed.stderr
            assert not (root / mode).exists() and not (root / "late-2").exists(), mode
        assert (root / "late-1").exists()
        session = next((root / "state" / "bart" / "sessions").rglob("session.jsonl")).read_text()
        results = [message["content"] for line in session.splitlines() for event in [json.loads(line)]
                   if event["event"] == "messages" for message in event["data"] if message["role"] == "tool_result"]
        assert [(r["call_id"], r["is_error"], r["output"].startswith("[not run: ")) for r in results] == [
            ("native-1", False, False), ("native-2", True, True)], results
        waiting = subprocess.Popen(args + ["--prompt", "wait-for-cancel"], cwd=root, env=env,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while not any(e["event"] == "waiting" for e in events(log)):
                assert waiting.poll() is None
                assert time.monotonic() < deadline
                time.sleep(0.01)
            stopped = next(e for e in events(log) if e["event"] == "waiting")
            waiting.send_signal(signal.SIGTERM)
            waiting.communicate(timeout=5)
            assert waiting.returncode == 143
            deadline = time.monotonic() + 5
            while any(running(pid) for pid in (stopped["pid"], stopped["child"])):
                assert time.monotonic() < deadline, stopped
                time.sleep(0.01)
        finally:
            if waiting.poll() is None:
                waiting.kill()
                waiting.communicate()


if "-p" in sys.argv:
    fixture()
else:
    check(sys.argv[1])
