"""Minimal MCP Streamable HTTP server used by compatibility and runtime tests.

Usage: mcp_http_fixture_server.py [--token TOKEN] [--log FILE] [--resources] [--dribble] [--hang-initialize]
Prints its base URL. `tools/call` answers with an event stream that also carries a
notification and a server request; every other method answers with plain JSON.
Requests need `Authorization: Bearer <token>` and, after `initialize`, the session
and protocol-version headers.

Sessions: the first is `fixture-session`, later ones `fixture-session-2`, ... The tool
`expire` ends the current session, so that the next request with it gets a 404;
`expire_for_good` also makes the server refuse to initialize again.
Streams: `resume` answers with an event stream that has ids and a `retry` field and
closes before the response; the client must resume it with a GET and `Last-Event-ID`
(the replay carries the response). `resume_twice` drops the replay once more.
`resume_fail` answers every resumption with a 500. `drop` closes the stream without ids.
`--resources` adds the `resources` capability and `resources/list`, `resources/read`.
`--dribble` writes event streams one byte at a time. `--hang-initialize` never answers in time.
Every request is logged: a cancellation with its params, a GET with its headers.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOLS = [
    {"name": "echo", "description": "Echo text.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
     "annotations": {"readOnlyHint": True}},
    {"name": "slow", "description": "Sleep.", "inputSchema": {"type": "object"}},
    {"name": "write", "description": "Echo text (write-capable).",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "expire", "description": "End the current session.", "inputSchema": {"type": "object"}},
    {"name": "expire_for_good", "description": "End the session and refuse new ones.",
     "inputSchema": {"type": "object"}},
    {"name": "resume", "description": "Answer over a stream that must be resumed.",
     "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
    {"name": "resume_twice", "description": "Drop the stream again after one resumption.",
     "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
    {"name": "resume_thrice", "description": "Drop the stream twice after resumptions.",
     "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
    {"name": "resume_primed", "description": "Only a priming event carries an id.",
     "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
    {"name": "resume_fail", "description": "Refuse every resumption.", "inputSchema": {"type": "object"}},
    {"name": "drop", "description": "Close the stream without ids.", "inputSchema": {"type": "object"}},
]
NATIVE = {"name": "read_resource", "description": "Native.", "inputSchema": {"type": "object"}}
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
NOTE = {"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "x"}}


def sse(events):
    """Event stream text; an event is an (id, retry, message) tuple, any part may be None."""
    out = ""
    for event_id, retry, message in events:
        if event_id is not None:
            out += f"id: {event_id}\n"
        if retry is not None:
            out += f"retry: {retry}\n"
        out += "event: message\n"
        out += f"data: {json.dumps(message)}\n\n" if message is not None else "data:\n\n"
    return out


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

    def stream(self, events):
        """An event stream that ends when the connection closes."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        data = sse(events).encode()
        if self.server.dribble:  # one byte at a time, so that every field is split across reads
            for index in range(len(data)):
                self.wfile.write(data[index:index + 1])
                self.wfile.flush()
                time.sleep(0.001)
        else:
            self.wfile.write(data)
            self.wfile.flush()
        self.close_connection = True

    def authorized(self):
        return self.headers.get("Authorization") == f"Bearer {self.server.token}"

    def session_alive(self):
        with self.server.lock:
            return self.headers.get("Mcp-Session-Id") == self.server.session

    def do_DELETE(self):
        self.record({"delete": self.headers.get("Mcp-Session-Id")})
        self.reply(200)

    def do_GET(self):
        if not self.authorized():
            self.reply(401)
            return
        resume = self.headers.get("Last-Event-ID")
        self.record({"get": {"last_event_id": resume, "session": self.headers.get("Mcp-Session-Id"),
                             "protocol": self.headers.get("MCP-Protocol-Version"),
                             "accept": self.headers.get("Accept")}})
        if resume is None:  # the optional stream for server messages is not offered
            self.reply(405)
            return
        pending = self.server.resumable.get(resume)
        if pending is None or pending["fail"]:
            self.reply(500)
            return
        pending["gets"] += 1
        if pending["gets"] <= pending["drops"]:
            next_id = f"r{pending['gets'] + 1}"
            self.server.resumable[next_id] = pending
            self.stream([(next_id, 20, NOTE)])
            return
        result = {"content": [{"type": "text", "text": "resumed"}]}
        self.stream([("r9", None, {"jsonrpc": "2.0", "id": pending["id"], "result": result})])

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
        entry = {"method": method, "id": body.get("id"), "session": self.headers.get("Mcp-Session-Id"),
                 "protocol": self.headers.get("MCP-Protocol-Version")}
        if method in ("notifications/cancelled", "resources/read", "tools/call"):
            entry["params"] = body.get("params")
        self.record(entry)
        if method != "initialize" and not self.session_alive():
            self.reply(404)
            return
        if "id" not in body:
            self.reply(202)
            return
        if method == "initialize" and self.server.hang_initialize:
            time.sleep(5)
            self.reply(503)
        elif method == "initialize" and self.server.refuse_initialize:
            self.reply(503)
        elif method == "initialize":
            with self.server.lock:
                self.server.sessions += 1
                count = self.server.sessions
                session = "fixture-session" if count == 1 else f"fixture-session-{count}"
                self.server.session = session
            capabilities = {"tools": {}}
            if self.server.resources:
                capabilities["resources"] = {}
            result = {"protocolVersion": "2025-06-18", "capabilities": capabilities,
                      "serverInfo": {"name": "fixture-http", "version": "1"}}
            self.json_reply(body["id"], result, [("Mcp-Session-Id", session)])
        elif method == "tools/list":
            tools = TOOLS + ([NATIVE] if self.server.native else [])
            self.json_reply(body["id"], {"tools": tools})
        elif method == "resources/list" and self.server.resources:
            cursor = (body.get("params") or {}).get("cursor")
            page = 1 if cursor == "page2" else 0
            result = {"resources": RESOURCES[page]}
            if page == 0:
                result["nextCursor"] = "page2"
            self.json_reply(body["id"], result)
        elif method == "resources/read" and self.server.resources:
            uri = body["params"]["uri"]
            if uri in CONTENTS:
                self.json_reply(body["id"], {"contents": CONTENTS[uri]})
            else:
                self.error_reply(body["id"], -32002, f"Resource not found: {uri}")
        elif method == "tools/call":
            self.call_tool(body)
        else:
            self.reply(400)

    def call_tool(self, body):
        name = body["params"]["name"]
        request_id = body["id"]
        if name == "slow":
            time.sleep(30)
        if name in ("expire", "expire_for_good"):
            with self.server.lock:
                self.server.session = None
                self.server.refuse_initialize = name == "expire_for_good"
        if name.startswith("resume"):
            drops = {"resume_twice": 1, "resume_thrice": 2}.get(name, 0)
            pending = {"id": request_id, "drops": drops, "fail": name == "resume_fail", "gets": 0}
            self.server.resumable["r0"] = self.server.resumable["r1"] = pending
            if name == "resume_primed":  # no event but the priming one carries an id
                self.stream([("r0", 20, None)])
            else:
                self.stream([("r0", 20, None), ("r1", None, NOTE)])
            return
        if name == "drop":
            self.stream([(None, None, NOTE)])
            return
        text = body["params"]["arguments"].get("text", "")
        events = [
            (None, None, NOTE),
            (None, None, {"jsonrpc": "2.0", "id": 900, "method": "ping"}),
            (None, None, {"jsonrpc": "2.0", "id": request_id,
                          "result": {"content": [{"type": "text", "text": text}]}}),
        ]
        self.reply(200, sse(events).encode(), "text/event-stream")

    def json_reply(self, request_id, result, headers=()):
        payload = json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}).encode()
        self.reply(200, payload, "application/json", headers)

    def error_reply(self, request_id, code, message):
        payload = json.dumps({"jsonrpc": "2.0", "id": request_id,
                              "error": {"code": code, "message": message}}).encode()
        self.reply(200, payload, "application/json")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--token", default="fixture-token")
    parser.add_argument("--log")
    parser.add_argument("--resources", action="store_true")
    parser.add_argument("--dribble", action="store_true")
    parser.add_argument("--hang-initialize", action="store_true")
    parser.add_argument("--native-resource-tool", action="store_true")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.token, server.log, server.resources = args.token, args.log, args.resources
    server.session, server.sessions, server.resumable = None, 0, {}
    server.refuse_initialize = False
    server.dribble = args.dribble
    server.native = args.native_resource_tool
    server.hang_initialize = args.hang_initialize
    server.entries, server.lock = [], threading.Lock()
    print(f"http://127.0.0.1:{server.server_port}/mcp", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
