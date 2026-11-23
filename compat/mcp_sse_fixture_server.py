"""Minimal MCP legacy HTTP+SSE fixture server (MCP 2024-11-05) for runtime and CLI tests.

Usage: mcp_sse_fixture_server.py [--token TOKEN] [--log FILE] [--bad-origin] [--prompts]
Prints `http://127.0.0.1:<port>/sse`.
GET /sse streams `event: endpoint` pointing to `/messages?session_id=sse-1` (or a cross-origin
URL when `--bad-origin` is set), then delivers JSON-RPC replies as `event: message`.
POST /messages accepts JSON-RPC requests/notifications and responds 202 Accepted.
"""

import argparse
import json
import queue
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOLS = [
    {
        "name": "echo",
        "description": "Echo text over legacy SSE.",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "slow",
        "description": "Sleep before responding.",
        "inputSchema": {"type": "object"},
    },
]
PROMPTS = [
    [
        {
            "name": "summarize",
            "description": "Summarize input",
            "arguments": [{"name": "topic", "description": "Topic", "required": True}],
        }
    ],
    [
        {
            "name": "bare",
        }
    ],
]
NOTE = {"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "x"}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def record(self, entry):
        with self.server.lock:
            self.server.entries.append(entry)
            if self.server.log:
                with open(self.server.log, "w", encoding="utf-8") as file:
                    json.dump(self.server.entries, file)

    def authorized(self):
        return self.headers.get("Authorization") == f"Bearer {self.server.token}"

    def reply(self, status, body=b"", content_type=None):
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if not self.authorized():
            self.reply(401)
            return
        if self.path != "/sse":
            self.reply(404)
            return
        q = queue.Queue()
        with self.server.lock:
            self.server.queues.append(q)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        endpoint = (
            "http://evil.example:9999/messages?session_id=sse-1"
            if self.server.bad_origin
            else "/messages?session_id=sse-1"
        )
        try:
            self.wfile.write(f"event: endpoint\ndata: {endpoint}\n\n".encode())
            self.wfile.flush()
            while True:
                item = q.get()
                if item is None:
                    break
                payload = f"event: message\ndata: {json.dumps(item)}\n\n".encode()
                self.wfile.write(payload)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            with self.server.lock:
                if q in self.server.queues:
                    self.server.queues.remove(q)
            self.close_connection = True

    def push_sse(self, message):
        with self.server.lock:
            queues = list(self.server.queues)
        for q in queues:
            q.put(message)

    def do_POST(self):
        if not self.authorized():
            self.reply(401)
            return
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length))
        if "method" not in body:
            self.record({"client_response": body})
            self.reply(202)
            return
        method = body["method"]
        entry = {"method": method, "id": body.get("id"), "path": self.path}
        if method in ("notifications/cancelled", "tools/call", "prompts/get"):
            entry["params"] = body.get("params")
        self.record(entry)
        self.reply(202)
        if "id" not in body:
            return
        req_id = body["id"]
        if method == "initialize":
            caps = {"tools": {}}
            if self.server.prompts:
                caps["prompts"] = {}
            self.push_sse(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": caps,
                        "serverInfo": {"name": "fixture-sse", "version": "1"},
                    },
                }
            )
        elif method == "tools/list":
            self.push_sse({"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}})
        elif method == "prompts/list" and self.server.prompts:
            cursor = (body.get("params") or {}).get("cursor")
            page = 1 if cursor == "p2" else 0
            res = {"prompts": PROMPTS[page]}
            if page == 0:
                res["nextCursor"] = "p2"
            self.push_sse({"jsonrpc": "2.0", "id": req_id, "result": res})
        elif method == "prompts/get" and self.server.prompts:
            name = (body.get("params") or {}).get("name")
            args = (body.get("params") or {}).get("arguments") or {}
            if name == "summarize":
                topic = args.get("topic", "general")
                self.push_sse(
                    {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "description": "Summary prompt",
                            "messages": [
                                {
                                    "role": "user",
                                    "content": {"type": "text", "text": f"Summarize {topic}"},
                                }
                            ],
                        },
                    }
                )
            else:
                self.push_sse(
                    {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {"code": -32602, "message": f"Unknown prompt: {name}"},
                    }
                )
        elif method == "tools/call":
            name = body["params"]["name"]
            if name == "slow":
                def delayed():
                    time.sleep(30)
                threading.Thread(target=delayed, daemon=True).start()
                return
            text = (body["params"].get("arguments") or {}).get("text", "")
            self.push_sse(NOTE)
            self.push_sse({"jsonrpc": "2.0", "id": 901, "method": "ping"})
            self.push_sse(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {"content": [{"type": "text", "text": f"sse:{text}"}]},
                }
            )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--token", default="fixture-token")
    parser.add_argument("--log")
    parser.add_argument("--bad-origin", action="store_true")
    parser.add_argument("--prompts", action="store_true")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.token = args.token
    server.log = args.log
    server.bad_origin = args.bad_origin
    server.prompts = args.prompts
    server.entries = []
    server.queues = []
    server.lock = threading.Lock()
    print(f"http://127.0.0.1:{server.server_port}/sse", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
