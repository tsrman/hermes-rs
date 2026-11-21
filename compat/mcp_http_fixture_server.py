"""Minimal MCP Streamable HTTP server used by compatibility and runtime tests.

Usage: mcp_http_fixture_server.py [--token TOKEN] [--log FILE]
Prints its base URL. `tools/call` answers with an event stream that also carries a
notification and a server request; every other method answers with plain JSON.
Requests need `Authorization: Bearer <token>` and, after `initialize`, the session
and protocol-version headers.
"""

import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SESSION = "fixture-session"
TOOLS = [
    {"name": "echo", "description": "Echo text.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "slow", "description": "Sleep.", "inputSchema": {"type": "object"}},
]


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

    def reply(self, status, body=b"", content_type=None, headers=()):
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def authorized(self):
        return self.headers.get("Authorization") == f"Bearer {self.server.token}"

    def do_DELETE(self):
        self.record({"delete": self.headers.get("Mcp-Session-Id")})
        self.reply(200)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if not self.authorized():
            self.reply(401)
            return
        if "method" not in body:  # the client's answer to a server request
            self.record({"client_response": body})
            self.reply(202)
            return
        method = body["method"]
        self.record({"method": method, "session": self.headers.get("Mcp-Session-Id"),
                     "protocol": self.headers.get("MCP-Protocol-Version")})
        if method != "initialize" and self.headers.get("Mcp-Session-Id") != SESSION:
            self.reply(404)
            return
        if "id" not in body:
            self.reply(202)
            return
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "fixture-http", "version": "1"}}
            self.json_reply(body["id"], result, [("Mcp-Session-Id", SESSION)])
        elif method == "tools/list":
            self.json_reply(body["id"], {"tools": TOOLS})
        elif method == "tools/call":
            name = body["params"]["name"]
            if name == "slow":
                time.sleep(30)
            text = body["params"]["arguments"].get("text", "")
            events = [
                {"jsonrpc": "2.0", "method": "notifications/message", "params": {}},
                {"jsonrpc": "2.0", "id": 900, "method": "ping"},
                {"jsonrpc": "2.0", "id": body["id"],
                 "result": {"content": [{"type": "text", "text": text}]}},
            ]
            payload = "".join(f"event: message\ndata: {json.dumps(event)}\n\n" for event in events)
            self.reply(200, payload.encode(), "text/event-stream")
        else:
            self.reply(400)

    def json_reply(self, request_id, result, headers=()):
        payload = json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}).encode()
        self.reply(200, payload, "application/json", headers)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--token", default="fixture-token")
    parser.add_argument("--log")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.token, server.log = args.token, args.log
    server.entries, server.lock = [], threading.Lock()
    print(f"http://127.0.0.1:{server.server_port}/mcp", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
