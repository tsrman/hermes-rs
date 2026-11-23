#!/usr/bin/env python3
"""Example custom Python extension implemented as an MCP stdio server.

This server uses only the Python standard library and demonstrates the supported
out-of-process extension contract for `hermes`:
- versioned JSON-RPC 2.0 handshake (`protocolVersion: 2025-06-18`)
- read-only tools (`word_stats`, `env_audit`) and a cancellable tool (`slow_task`)
- resource discovery and reading (`plugin://status`)
- cooperative cancellation via `notifications/cancelled`
"""

from __future__ import annotations

import json
import os
import sys
import threading


PROTOCOL_VERSION = "2025-06-18"
WRITE_LOCK = threading.Lock()
CANCELLED: dict[object, threading.Event] = {}
LOG_PATH = os.environ.get("HERMES_PLUGIN_LOG")


TOOLS = [
    {
        "name": "word_stats",
        "description": "Count lines and words in the supplied text.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "env_audit",
        "description": "Inspect which environment variables reached the plugin process.",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "slow_task",
        "description": "Wait until cancelled or timeout.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]

RESOURCES = [
    {
        "uri": "plugin://status",
        "name": "status",
        "description": "Static status of the example Python extension.",
        "mimeType": "text/plain",
    }
]


def send(message: dict) -> None:
    with WRITE_LOCK:
        sys.stdout.write(json.dumps(message, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def log(entry: dict) -> None:
    if LOG_PATH:
        with open(LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry, ensure_ascii=False) + "\n")


def run_slow(request_id: object) -> None:
    event = CANCELLED.setdefault(request_id, threading.Event())
    if not event.wait(30):
        send({"jsonrpc": "2.0", "id": request_id, "result": {"content": [{"type": "text", "text": "finished"}]}})


def call_tool(name: str, arguments: dict) -> dict:
    if name == "word_stats":
        text = str(arguments.get("text", ""))
        stats = {"lines": len(text.splitlines()), "words": len(text.split())}
        return {"content": [{"type": "text", "text": json.dumps(stats)}]}
    if name == "env_audit":
        report = {
            "has_hermes_api_key": "HERMES_API_KEY" in os.environ,
            "has_hermes_home": "HERMES_HOME" in os.environ,
            "has_unrelated_secret": "HERMES_UNRELATED_SECRET" in os.environ,
            "plugin_token": os.environ.get("PLUGIN_TOKEN"),
        }
        return {"content": [{"type": "text", "text": json.dumps(report)}]}
    return {"isError": True, "content": [{"type": "text", "text": f"unknown tool: {name}"}]}


def main() -> None:
    log({"event": "started", "pid": os.getpid()})
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        message = json.loads(line)
        method = message.get("method")
        if method in ("tools/call", "resources/read", "notifications/cancelled"):
            log({"method": method, "id": message.get("id"), "params": message.get("params")})

        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "python-mcp-plugin", "version": "1.0.0"},
                },
            })
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"tools": TOOLS}})
        elif method == "resources/list":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"resources": RESOURCES}})
        elif method == "resources/read":
            uri = message.get("params", {}).get("uri")
            if uri == "plugin://status":
                send({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {
                        "contents": [
                            {"uri": uri, "mimeType": "text/plain", "text": "python-mcp-plugin ok"}
                        ]
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "error": {"code": -32002, "message": f"Resource not found: {uri}"},
                })
        elif method == "notifications/cancelled":
            req_id = message.get("params", {}).get("requestId")
            CANCELLED.setdefault(req_id, threading.Event()).set()
        elif method == "tools/call":
            params = message.get("params", {})
            name = params.get("name", "")
            if name == "slow_task":
                threading.Thread(target=run_slow, args=(message["id"],), daemon=True).start()
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": call_tool(name, params.get("arguments") or {}),
                })


if __name__ == "__main__":
    main()
