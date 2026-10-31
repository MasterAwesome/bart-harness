"""A stdio MCP server for Bart's tests: FIXTURE_MODERN selects 2026-07-28
(`server/discover`), otherwise 2025-11-25 (`initialize`). Each mode rejects
 the other's handshake. Tools come in two pages. `spawn` starts a grandchild that should die with
the server tree, even in a new session."""
import json
import os
import subprocess
import sys
import time

print("fixture server starting", file=sys.stderr, flush=True)
if os.environ.get("FIXTURE_FAIL"):
    token = os.environ.get("API_TOKEN", "")
    sys.stderr.write(token[:4097])
    sys.stderr.flush()
    if os.environ.get("FIXTURE_SPLIT"):
        time.sleep(0.2)
    print(token[4097:], file=sys.stderr, flush=True)
    print("cannot start: FIXTURE_FAIL is set", file=sys.stderr, flush=True)
    sys.exit(3)

if os.environ.get("FIXTURE_PID_FILE"):
    with open(os.environ["FIXTURE_PID_FILE"] + ".tmp", "w") as pid_file:
        pid_file.write(str(os.getpid()))
    os.replace(os.environ["FIXTURE_PID_FILE"] + ".tmp", os.environ["FIXTURE_PID_FILE"])
if os.environ.get("FIXTURE_START_HANG"):
    time.sleep(300)

if os.environ.get("FIXTURE_START_GATE"):
    while not os.path.exists(os.environ["FIXTURE_START_GATE"]):
        time.sleep(.01)

PAGES = [
    [{"name": "echo", "description": "Echoes its arguments.\nMore detail.",
      "inputSchema": {"type": "object", "properties": {"payload": {"description": "A zebra-striped value.",
                                                        "properties": {"inner": {"description": "A kiwi-shaped part."}}}}}}],
    [{"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}},
     {"name": "spawn", "description": "Starts a sleeping grandchild; returns its pid.", "inputSchema": {"type": "object"}},
     {"name": "hang", "description": "Never answers.", "inputSchema": {"type": "object"}},
     {"name": "mixed", "description": "Structured data with extra content.", "inputSchema": {"type": "object"}},
     {"name": "broken", "description": "Fails with a picture.", "inputSchema": {"type": "object"}},
     {"name": "env", "description": "Reports its environment.", "inputSchema": {"type": "object"}}],
]
PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="


def call(name, arguments):
    if os.environ.get("FIXTURE_CALL_FILE"):
        with open(os.environ["FIXTURE_CALL_FILE"], "a") as calls:
            calls.write(name + "\n")
    if name == "echo":
        return {"content": [{"type": "text", "text": json.dumps(arguments)}], "structuredContent": arguments}
    if name == "mixed":
        data = {"rows": 2}
        return {"structuredContent": data, "content": [
            {"type": "text", "text": json.dumps(data)},
            {"type": "text", "text": "note: rows are capped"},
            {"type": "image", "data": arguments.get("image", PIXEL), "mimeType": arguments.get("mime", "image/png")},
            {"type": "resource_link", "uri": "file:///report.csv", "name": "report"},
            {"type": "audio", "data": "AA==", "mimeType": "audio/wav"},
            {"type": "resource", "resource": {"uri": "file:///raw", "blob": "AA==", "mimeType": "application/octet-stream"}},
            {"type": "resource_link", "uri": "file:///odd\nname" + "x" * 5000, "name": "odd"}]}
    if name == "broken":
        return {"isError": True, "content": [{"type": "text", "text": "render failed"},
                                             {"type": "image", "data": PIXEL, "mimeType": "image/png"}]}
    if name == "env":
        import socket
        network = False
        if os.environ.get("FIXTURE_NETWORK_PORT"):
            try:
                with socket.create_connection(("127.0.0.1", int(os.environ["FIXTURE_NETWORK_PORT"])), timeout=1):
                    network = True
            except OSError:
                pass
        data = {"network": network, "secret": os.environ.get("FIXTURE_SECRET"), "empty": os.environ.get("FIXTURE_EMPTY"),
                "big": len(os.environ.get("FIXTURE_BIG", "")),
                "home": [os.access(os.environ.get("FIXTURE_HOME", "/nonexistent"), mode) for mode in (os.R_OK, os.W_OK)],
                "env_fds": [os.readlink(f"/proc/self/fd/{fd}") for fd in os.listdir("/proc/self/fd")
                            if os.path.exists(f"/proc/self/fd/{fd}") and "bart-mcp-env" in os.readlink(f"/proc/self/fd/{fd}")]}
        return {"structuredContent": data, "content": [{"type": "text", "text": json.dumps(data)}]}
    if name == "spawn":
        child = subprocess.Popen(["sleep", os.environ.get("FIXTURE_SLEEP", "300")], start_new_session=sys.platform == "linux")
        return {"content": [{"type": "text", "text": str(child.pid)}]}
    return {"content": [{"type": "text", "text": "it failed"}], "isError": True}


for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method, params = message.get("method"), message.get("params") or {}
    if method == "server/discover" and os.environ.get("FIXTURE_MODERN"):
        result = {"resultType": "complete", "supportedVersions": ["2026-07-28"],
                  "capabilities": {"tools": {}}, "ttlMs": 0, "cacheScope": "private",
                  "instructions": os.environ.get("FIXTURE_GUIDANCE", "A test server.\nSecond line."),
                  "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fixture", "version": "1"}}}
    elif method == "initialize" and not os.environ.get("FIXTURE_MODERN"):
        result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fixture", "version": "1"}, "instructions": os.environ.get("FIXTURE_GUIDANCE", "A test server.\nSecond line.")}
    elif method == "tools/list":
        page = int(params.get("cursor") or 0)
        if os.environ.get("FIXTURE_MANY_TOOLS"):
            result = {"tools": [{"name": f"tool{i}", "description": "A numbered tool.",
                                 "inputSchema": {"type": "object", "properties": {
                                     "x": {"description": "the unique quokka knob" if i == 650 else "a value"}}}}
                                for i in range(700)]}
        else:
            result = {"tools": PAGES[page]}
            if page + 1 < len(PAGES):
                result["nextCursor"] = str(page + 1)
    elif method == "tools/call" and params["name"] == "hang":
        continue  # never answers, while other requests are still served
    elif method == "tools/call":
        result = call(params["name"], params.get("arguments") or {})
    else:
        reply = {"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": f"no method {method}"}}
        print(json.dumps(reply), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
