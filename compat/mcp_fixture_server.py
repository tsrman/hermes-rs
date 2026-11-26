"""Minimal MCP stdio server used by compatibility and runtime tests.

HERMES_MCP_LOG names a file that receives one JSON line for every `tools/call`, `resources/read`
and `notifications/cancelled` the server gets (with its params). HERMES_MCP_SLOW_LIST=1 makes `tools/list` take 30 s. HERMES_MCP_EXTRA=1 adds tools whose names collide after sanitising and tools that answer with an
empty `isError`, `structuredContent` and `_meta`. HERMES_MCP_RESOURCES=1 adds the
`resources` capability with `resources/list` and `resources/read`. The `slow` tool runs beside
the request loop, so a cancellation reaches the server; a cancelled call is never answered.
"""

import json
import os
import subprocess
import sys
import threading
import time

# MCP speaks UTF-8; Windows would otherwise decode stdin with the locale code page.
sys.stdin.reconfigure(encoding="utf-8")

TOOLS = [
    [
        {"name": "echo", "description": "Echo text.",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
         "annotations": {"readOnlyHint": True}},
        {"name": "fail", "description": "Report a tool error.", "inputSchema": {"type": "object"}},
    ],
    [
        {"name": "env", "description": "Report environment flags.", "inputSchema": {"type": "object"}},
        {"name": "slow", "description": "Sleep.", "inputSchema": {"type": "object"}},
        {"name": "spawn-child", "description": "Start a grandchild.", "inputSchema": {"type": "object"}},
        {"name": "image", "description": "Return non-text content.", "inputSchema": {"type": "object"}},
    ],
]

if os.environ.get("HERMES_MCP_MEDIA") == "1":
    TOOLS[1] += [
        {"name": "png", "description": "Return valid PNG image.", "inputSchema": {"type": "object"}},
        {"name": "huge", "description": "Return multi-line oversized output.", "inputSchema": {"type": "object"}},
    ]


EXTRA = os.environ.get("HERMES_MCP_EXTRA") == "1"
if EXTRA:
    TOOLS[1] += [
        {"name": "dup-x", "description": "Collides with dup_x.", "inputSchema": {"type": "object"}},
        {"name": "dup_x", "description": "Collides with dup-x.", "inputSchema": {"type": "object"}},
        {"name": "list.resources", "description": "Collides with list-resources.", "inputSchema": {"type": "object"}},
        {"name": "list-resources", "description": "Collides with list.resources.", "inputSchema": {"type": "object"}},
        {"name": "echo", "description": "Listed twice.", "inputSchema": {"type": "object"}},
        {"name": "empty-error", "description": "isError without content.", "inputSchema": {"type": "object"}},
        {"name": "rich", "description": "structuredContent and _meta.", "inputSchema": {"type": "object"}},
    ]


RESOURCES = [
    [{"uri": "file:///notes/a.txt", "name": "a.txt", "description": "First note",
      "mimeType": "text/plain"},
     {"uri": "mem://plain", "name": "plain"}],
    [{"uri": "file:///notes/b.bin", "name": "b.bin", "mimeType": "application/octet-stream"}],
]
CONTENTS = {
    "file:///notes/a.txt": [{"uri": "file:///notes/a.txt", "mimeType": "text/plain", "text": "alpha"}],
    "mem://plain": [{"uri": "mem://plain", "text": "one"}, {"uri": "mem://plain", "text": "two"}],
    "file:///notes/b.bin": [{"uri": "file:///notes/b.bin", "mimeType": "application/octet-stream",
                             "blob": "AAECAw=="}],
}
RESOURCES_ON = os.environ.get("HERMES_MCP_RESOURCES") == "1"
LOG = os.environ.get("HERMES_MCP_LOG")
WRITE = threading.Lock()
CANCELLED = {}


def send(message):
    with WRITE:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def log(entry):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as file:
            file.write(json.dumps(entry) + "\n")


def call(name, arguments):
    if name == "echo":
        return {"content": [{"type": "text", "text": arguments.get("text", "")}]}
    if name == "fail":
        return {"isError": True, "content": [{"type": "text", "text": "tool failed"}]}
    if name == "env":
        flags = {"parent_secret": "HERMES_TEST_SECRET" in os.environ,
                 "configured": os.environ.get("HERMES_TEST_CONFIGURED")}
        return {"content": [{"type": "text", "text": json.dumps(flags)}]}
    if name == "slow":
        return None
    if name == "spawn-child":
        child = subprocess.Popen(["sleep", "300"])
        return {"content": [{"type": "text", "text": str(child.pid)}]}
    if name == "empty-error":
        return {"isError": True, "content": []}
    if name == "rich":
        return {"content": [{"type": "text", "text": "summary"}], "structuredContent": {"n": 1, "ok": True},
                "_meta": {"com.example/k": "v", "modelcontextprotocol.io/r": 1}}
    if name == "image":
        return {"content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]}
    if name == "png":
        return {"content": [{"type": "image", "data": "iVBORw0KGgoAAAAAAAAAAAAAAAAAAAAA", "mimeType": "image/png"}]}
    if name == "huge":
        lines = "\n".join(f"row {i:04d} payload" for i in range(60)) + "\n"
        return {"content": [{"type": "text", "text": lines}]}
    return {"isError": True, "content": [{"type": "text", "text": "unknown tool"}]}


def slow(request_id):
    cancelled = CANCELLED.setdefault(request_id, threading.Event())
    if not cancelled.wait(30):
        send({"jsonrpc": "2.0", "id": request_id, "result": {"content": []}})


log({"pid": os.getpid()})
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method in ("tools/call", "resources/read", "notifications/cancelled"):
        log({"method": method, "id": message.get("id"), "params": message.get("params")})
    if method == "initialize":
        capabilities = {"tools": {}}
        if RESOURCES_ON:
            capabilities["resources"] = {}
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "protocolVersion": "2025-06-18", "capabilities": capabilities,
            "serverInfo": {"name": "fixture", "version": "1"}}})
    elif method == "tools/list":
        if os.environ.get("HERMES_MCP_SLOW_LIST") == "1":
            time.sleep(30)
        cursor = message.get("params", {}).get("cursor")
        page = 1 if cursor == "page2" else 0
        result = {"tools": TOOLS[page]}
        if page == 0:
            result["nextCursor"] = "page2"
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "resources/list" and RESOURCES_ON:
        page = 1 if message.get("params", {}).get("cursor") == "page2" else 0
        result = {"resources": RESOURCES[page]}
        if page == 0:
            result["nextCursor"] = "page2"
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "resources/read" and RESOURCES_ON:
        uri = message["params"]["uri"]
        if uri in CONTENTS:
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"contents": CONTENTS[uri]}})
        else:
            send({"jsonrpc": "2.0", "id": message["id"],
                  "error": {"code": -32002, "message": f"Resource not found: {uri}"}})
    elif method == "notifications/cancelled":
        CANCELLED.setdefault(message["params"]["requestId"], threading.Event()).set()
    elif method == "tools/call":
        # A notification and a server request must not confuse the client.
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "x"}})
        send({"jsonrpc": "2.0", "id": 900, "method": "ping"})
        params = message["params"]
        if params["name"] == "slow":
            threading.Thread(target=slow, args=(message["id"],), daemon=True).start()
        else:
            send({"jsonrpc": "2.0", "id": message["id"],
                  "result": call(params["name"], params.get("arguments", {}))})
