#!/usr/bin/env python3
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading

CODE = "print('tool-' + 'ran')"
requests = []


def sse(events):
    return b"".join(b"data: " + json.dumps(event).encode() + b"\n\n" for event in events)


def responses(called, text):
    response = {"id": "resp", "object": "response", "created_at": 1, "model": "fixture", "status": "in_progress",
                "error": None, "incomplete_details": None, "output": []}
    if called:
        item = {"id": "msg", "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}]}
        events = [
            {"type": "response.output_item.added", "output_index": 0,
             "item": {**item, "status": "in_progress", "content": []}},
            {"type": "response.content_part.added", "output_index": 0, "item_id": "msg", "content_index": 0,
             "part": {"type": "output_text", "text": "", "annotations": []}},
            {"type": "response.output_text.delta", "output_index": 0, "item_id": "msg", "content_index": 0, "delta": text},
            {"type": "response.output_text.done", "output_index": 0, "item_id": "msg", "content_index": 0, "text": text},
            {"type": "response.content_part.done", "output_index": 0, "item_id": "msg", "content_index": 0,
             "part": item["content"][0]},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
        ]
    else:
        arguments = json.dumps({"code": CODE})
        item = {"id": "fc", "type": "function_call", "status": "completed", "call_id": "call", "name": "python", "arguments": arguments}
        events = [
            {"type": "response.output_item.added", "output_index": 0, "item": {**item, "status": "in_progress", "arguments": ""}},
            {"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc", "delta": arguments},
            {"type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc", "name": "python", "arguments": arguments},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
        ]
    done = {**response, "status": "completed", "output": [item],
            "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}}
    events = [{"type": "response.created", "response": response}, *events,
              {"type": "response.completed", "response": done}]
    return sse([{**event, "sequence_number": number} for number, event in enumerate(events)])


def chat(called, text):
    def chunk(delta, **extra):
        return {"choices": [{"index": 0, "delta": delta, **extra}]}
    if called:
        events = [chunk({"role": "assistant", "content": text}), chunk({}, finish_reason="stop")]
    else:
        call = {"index": 0, "id": "call", "type": "function", "function": {"name": "python", "arguments": json.dumps({"code": CODE})}}
        events = [chunk({"role": "assistant", "tool_calls": [call]}), chunk({}, finish_reason="tool_calls")]
    return sse(events) + b"data: [DONE]\n\n"


def messages(called, text):
    start = {"type": "message_start", "message": {"id": "msg", "model": "fixture",
             "usage": {"input_tokens": 5, "output_tokens": 1}}}
    if called:
        block = {"type": "text", "text": ""}
        delta = {"type": "text_delta", "text": text}
        stop = "end_turn"
    else:
        block = {"type": "tool_use", "id": "call", "name": "python", "input": {}}
        delta = {"type": "input_json_delta", "partial_json": json.dumps({"code": CODE})}
        stop = "tool_use"
    return sse([start,
                {"type": "content_block_start", "index": 0, "content_block": block},
                {"type": "content_block_delta", "index": 0, "delta": delta},
                {"type": "content_block_stop", "index": 0},
                {"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 3}},
                {"type": "message_stop"}])


def gemini(called, text):
    part = {"text": text} if called else {"functionCall": {"id": "call", "name": "python", "args": {"code": CODE}}}
    return sse([{"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP", "index": 0}],
                 "responseId": "resp", "modelVersion": "fixture",
                 "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2}}])


PROTOCOLS = {"responses": responses, "chat": chat, "messages": messages, "gemini": gemini}


class Server(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'{"data":[],"models":[],"has_more":false}')

    def do_POST(self):
        protocol = self.path.split("/")[1]
        body = self.rfile.read(int(self.headers["Content-Length"])).decode()
        called = "tool-ran" in body
        requests.append((protocol, called, json.loads(body)))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.write(PROTOCOLS[protocol](called, f"{protocol}-done"))


def check(binary):
    with tempfile.TemporaryDirectory(prefix="bart-provider-http-") as root:
        root = pathlib.Path(root)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_port}"
        paths = {"responses": "/v1", "chat": "/v1", "messages": "/v1", "gemini": "/v1beta"}
        config = root / "config/bart"
        config.mkdir(parents=True)
        (config / "providers.json").write_text(json.dumps(
            {name: {"protocol": name, "url": f"{base}/{name}{path}", "key": False} for name, path in paths.items()}))
        (config / "settings.json").write_text(json.dumps(
            {"models": {name: {"fixture": {"context_window": 100000}} for name in paths}}))
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"))
        try:
            for protocol in PROTOCOLS:
                requests.clear()
                run = subprocess.run([binary, "--temporary", "--approval", "none", "--provider", protocol,
                                      "--model", "fixture", "--prompt", "go"],
                                     cwd=root, env=env, capture_output=True, timeout=60)
                assert run.returncode == 0 and run.stdout.split() == [f"{protocol}-done".encode()], (protocol, run)
                assert [called for _, called, _ in requests] == [False, True], (protocol, requests)
        finally:
            server.shutdown()


check(sys.argv[1])
