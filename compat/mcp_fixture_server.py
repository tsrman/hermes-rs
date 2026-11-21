"""Minimal MCP stdio server used by compatibility and runtime tests."""

import json
import os
import subprocess
import sys
import time

TOOLS = [
    [
        {"name": "echo", "description": "Echo text.",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
        {"name": "fail", "description": "Report a tool error.", "inputSchema": {"type": "object"}},
    ],
    [
        {"name": "env", "description": "Report environment flags.", "inputSchema": {"type": "object"}},
        {"name": "slow", "description": "Sleep.", "inputSchema": {"type": "object"}},
        {"name": "spawn-child", "description": "Start a grandchild.", "inputSchema": {"type": "object"}},
        {"name": "image", "description": "Return non-text content.", "inputSchema": {"type": "object"}},
    ],
]


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


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
        time.sleep(30)
        return {"content": []}
    if name == "spawn-child":
        child = subprocess.Popen(["sleep", "300"])
        return {"content": [{"type": "text", "text": str(child.pid)}]}
    if name == "image":
        return {"content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]}
    return {"isError": True, "content": [{"type": "text", "text": "unknown tool"}]}


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1"}}})
    elif method == "tools/list":
        cursor = message["params"].get("cursor")
        page = 1 if cursor == "page2" else 0
        result = {"tools": TOOLS[page]}
        if page == 0:
            result["nextCursor"] = "page2"
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "tools/call":
        # A notification and a server request must not confuse the client.
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}})
        send({"jsonrpc": "2.0", "id": 900, "method": "ping"})
        params = message["params"]
        send({"jsonrpc": "2.0", "id": message["id"],
              "result": call(params["name"], params.get("arguments", {}))})
