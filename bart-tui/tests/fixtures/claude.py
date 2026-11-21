#!/usr/bin/env python3
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time


def send(value):
    print(json.dumps(value), flush=True)


def record(event, **fields):
    with open(os.environ["FIXTURE_LOG"], "a") as log:
        log.write(json.dumps(dict(event=event, pid=os.getpid(), **fields)) + "\n")


def callback(index, code=None, name="python", arguments=None):
    send({"type": "control_request", "request_id": f"callback-{index}", "request": {
        "subtype": "mcp_message", "server_name": "bart", "message": {
            "jsonrpc": "2.0", "id": index, "method": "tools/call", "params": {
                "name": name, "arguments": arguments if arguments is not None else {"code": code or CODES[index]},
                "_meta": {"claudecode/toolUseId": f"native-{index}", "progressToken": 7}}}}})


MESSAGE = 0


def response(text=None, codes=None, before_stop=lambda: None):
    global MESSAGE
    MESSAGE += 1
    message_id = f"message-{MESSAGE}"
    codes = codes or CODES
    send({"type": "stream_event", "event": {"type": "message_start", "message": {
        "id": message_id, "model": "fixture", "usage": {"input_tokens": 40000 if text == "NEXT" else 10,
        "cache_read_input_tokens": 30, "cache_creation_input_tokens": 4, "output_tokens": 0}}}})
    offset = 7 if MODE == "observed-only" else 0
    if offset:
        send({"type": "stream_event", "event": {"type": "content_block_start", "index": 2,
              "content_block": {"type": "future_block", "data": "ignored"}}})
        send({"type": "stream_event", "event": {"type": "content_block_delta", "index": 2,
              "delta": {"type": "future_delta", "text": "must not display"}}})
        send({"type": "stream_event", "event": {"type": "content_block_stop", "index": 2}})
    for index in ([1] if text else sorted(codes)):
        block = {"type": "text", "text": ""} if text else {
            "type": "tool_use", "id": f"native-{index}", "name": "mcp__bart__python", "input": {}}
        send({"type": "stream_event", "event": {"type": "content_block_start", "index": index - 1 + offset, "content_block": block}})
        delta = {"type": "text_delta", "text": text} if text else {
            "type": "input_json_delta", "partial_json": '{"code":' if MODE == "observed-only" else json.dumps({"code": codes[index]})}
        send({"type": "stream_event", "event": {"type": "content_block_delta", "index": index - 1 + offset, "delta": delta}})
        final = {"type": "text", "text": text} if text else {
            "type": "tool_use", "id": f"native-{index}", "name": "mcp__bart__python", "input": {"code": codes[index]}}
        send({"type": "assistant", "message": {"id": message_id, "model": "fixture", "role": "assistant",
              "content": [final], "usage": {"input_tokens": 10, "output_tokens": 6}}})
        send({"type": "stream_event", "event": {"type": "content_block_stop", "index": index - 1 + offset}})
    send({"type": "stream_event", "event": {"type": "message_delta", "delta": {
        "stop_reason": ("refusal" if text == "REFUSED" or not text and (codes is CODES or MODE == "observed-only")
                        else "end_turn" if text else "tool_use")}, "usage": {"output_tokens": 6}}})
    before_stop()
    if not text and codes is CODES:
        callback(1)
    send({"type": "stream_event", "event": {"type": "message_stop"}})
    if text:
        send({"type": "result", "subtype": "success", "is_error": False, "total_cost_usd": 0.01})


MODE = None


def marker(name):
    return f'from pathlib import Path\nPath("{name}").write_text("executed")'


def scenario(mode):
    global MODE
    MODE = mode
    if mode == "cancelled":
        callback(1, 'from pathlib import Path\nimport time\nPath("cancellation-started").write_text("started")\ntime.sleep(30)\nPath("cancelled").write_text("must not happen")')
        def cancel():
            deadline = time.monotonic() + 5
            while not Path("cancellation-started").exists():
                assert time.monotonic() < deadline
                time.sleep(0.01)
            for _ in range(32):
                send({"type": "tool_progress", "tool_use_id": "native-1"})
            send({"type": "control_cancel_request", "request_id": "callback-1"})
            response("CANCELLED")
        threading.Thread(target=cancel, daemon=True).start()
    elif mode == "normalized":
        code = marker("provisional") + '\nprint("\\u00ad")'
        response(codes={1: code})
        callback(1, marker("authoritative") + '\nprint("\u00ad")')
    elif mode == "observed-only":
        response(codes={1: marker(mode)})
        response("OBSERVED")
    elif mode == "callback-only":
        callback(1, marker(mode) + '\nprint("CALLBACK")')
    elif mode == "refusal":
        response("REFUSED")
    elif mode == "tool-errors":
        callback(1, 'print("unused")', name="unregistered")
    elif mode == "duplicate":
        send({"type": "assistant", "message": {"id": "redacted", "model": "fixture",
              "content": [{"type": "redacted_thinking", "data": "opaque-fixture"}],
              "usage": {"input_tokens": 10, "output_tokens": 0}}})
        callback(1, marker(mode) + '\nprint("FIRST")')


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
            elif text.strip().startswith("scenario-"):
                scenario(text.strip().removeprefix("scenario-"))
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
            if MODE == "normalized":
                assert "\u00ad" in result["result"]["content"][0]["text"]
                response("DONE")
                continue
            if MODE == "callback-only":
                assert "CALLBACK" in result["result"]["content"][0]["text"]
                response("DONE")
                continue
            if MODE == "cancelled":
                raise AssertionError("cancelled request received a reply")
            if MODE == "tool-errors":
                assert result["result"]["isError"], result
                if index == 1:
                    callback(2, arguments={"code": 123})
                elif index == 2:
                    callback(3, arguments={"code": "print('must not run')", "extra": True})
                else:
                    response("RECOVERED")
                continue
            if MODE == "duplicate":
                assert "FIRST" in result["result"]["content"][0]["text"]
                callback(1, marker("duplicate-again"))
                continue
            assert str(42 if index == 1 else 56) in result["result"]["content"][0]["text"]
            images = [block for block in result["result"]["content"] if block["type"] == "image"]
            assert images == ([{"type": "image", "data": PIXEL, "mimeType": "image/png"}] if index == 2 else []), images
            notes = [block for block in result["result"]["content"] if block.get("text", "").startswith("[Bart job: job[")]
            record("released", call=index, notes=len(notes))
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
        process = subprocess.Popen(args + ["--json"], text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=root)
        output, costs = [], []
        deadline = threading.Timer(30, process.kill)
        deadline.start()
        try:
            for message in ({"type": "user", "text": "go"}, {"type": "user", "text": "next"}, {"type": "compact"},
                            {"type": "user", "text": "after-checkpoint"}):
                process.stdin.write(json.dumps(message) + "\n")
                process.stdin.flush()
                while True:
                    event = json.loads(process.stdout.readline())
                    if event["type"] == "usage":
                        costs.append(event.get("cost"))
                    if event["type"] == "turn_done":
                        assert event["status"] == "completed", event
                        output.append(event["message"].strip())
                        break
            _, error = process.communicate(timeout=30)
        finally:
            deadline.cancel()
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                process.communicate(timeout=5)
        assert process.returncode == 0, error
        assert output == ["DONE", "NEXT", "", "AFTER"], output
        assert (root / "executions").read_text() == "ab"
        seen = events(log)
        mains = [e for e in seen if e["event"] == "start" and e["configured"] and not e["summary"]]
        assert len(mains) == 2, seen
        first = [e for e in seen if e["pid"] == mains[0]["pid"] and e["event"] == "input"]
        assert len(first) == 2, first
        assert len([e for e in seen if e["event"] == "released"]) == 2
        assert sum(e["notes"] for e in seen if e["event"] == "released") == 1
        last = [e for e in seen if e["pid"] == mains[1]["pid"] and e["event"] == "input"][0]
        assert "[Bart summary: replaces earlier context" in last["text"], last
        assert 0.01 in costs, costs
        limited = subprocess.run(args + ["--prompt", "limited"], cwd=root, env=env, text=True,
                                 stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        assert limited.returncode == 0 and limited.stdout.split() == ["RESUMED"], limited
        assert "HTTP 429 Too Many Requests: You've hit your limit" in limited.stderr, limited.stderr
        assert "for reset, then continuing" in limited.stderr, limited.stderr
        for mode, expected in [("normalized", "DONE"), ("callback-only", "DONE"),
                               ("observed-only", "OBSERVED"), ("cancelled", "CANCELLED"),
                               ("tool-errors", "RECOVERED")]:
            completed = subprocess.run(args + ["--prompt", f"scenario-{mode}"], cwd=root, env=env,
                                       text=True, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
            assert completed.returncode == 0 and completed.stdout.strip() == expected, completed
        assert (root / "authoritative").read_text() == "executed"
        assert (root / "callback-only").read_text() == "executed"
        assert not (root / "provisional").exists() and not (root / "observed-only").exists()
        assert (root / "cancellation-started").exists() and not (root / "cancelled").exists()
        persistent = args[:1] + args[2:]
        failed = subprocess.run(persistent + ["--prompt", "scenario-duplicate"], cwd=root, env=env,
                                text=True, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        assert failed.returncode != 0 and "duplicate Claude Code tool request" in failed.stderr, failed
        assert (root / "duplicate").exists() and not (root / "duplicate-again").exists()
        session = next((root / "state" / "bart" / "sessions").rglob("session.jsonl")).read_text()
        results = [message["content"] for line in session.splitlines() for event in [json.loads(line)]
                   if event["event"] == "messages" for message in event["data"] if message["role"] == "tool_result"]
        assert [(r["call_id"], r["is_error"]) for r in results] == [("native-1", False)], results
        redacted = [part["replay"] for line in session.splitlines() for event in [json.loads(line)]
                    if event["event"] == "messages" for message in event["data"] if message["role"] == "assistant"
                    for part in message["content"]["parts"] if (part.get("replay") or {}).get("type") == "redacted_thinking"]
        assert redacted == [{"type": "redacted_thinking", "data": "opaque-fixture"}], redacted
        assert "opaque-fixture" not in failed.stdout + failed.stderr
        for mode, expected_refusal, expected_text in [("observed-only", False, "OBSERVED"), ("refusal", True, "REFUSED")]:
            result = subprocess.run(persistent + ["--prompt", f"scenario-{mode}"], cwd=root, env=env,
                                    text=True, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
            assert result.returncode == 0 and result.stdout.strip() == expected_text, result
            journals = [[json.loads(line) for line in path.read_text().splitlines()]
                        for path in (root / "state" / "bart" / "sessions").rglob("session.jsonl")]
            matching = [journal for journal in journals if any(
                message["role"] == "user" and message["content"] == f"scenario-{mode}"
                for event in journal if event["event"] == "messages" for message in event["data"])]
            assert len(matching) == 1, matching
            final = [message["content"] for event in matching[0] if event["event"] == "messages"
                     for message in event["data"] if message["role"] == "assistant"][-1]
            assert final["refused"] is expected_refusal, final
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
