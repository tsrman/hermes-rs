#!/usr/bin/env python3
"""Loopback-only OpenAI-compatible server for deterministic parity tests."""

import argparse
import json
import ssl
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def send_json(self, status, value):
        data = json.dumps(value, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/requests":
            self.send_json(200, self.server.requests)
        elif self.path == "/authorizations":
            self.send_json(200, getattr(self.server, "authorizations", []))
        elif self.path == "/release" and self.server.scenario in ("stall_on_resume", "compact"):
            self.server.release.set()
            self.send_json(200, {})
        else:
            self.send_json(404, {"error": "unknown endpoint"})

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_json(404, {"error": "unknown endpoint"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length < 0 or length > 1_048_576:
                raise ValueError("invalid request length")
            body = json.loads(self.rfile.read(length))
            if not isinstance(body, dict):
                raise ValueError("request must be an object")
        except (ValueError, UnicodeDecodeError):
            self.send_json(400, {"error": "invalid JSON request"})
            return
        self.server.requests.append(body)
        # Servers built by other tests do not define the list.
        if not hasattr(self.server, "authorizations"):
            self.server.authorizations = []
        self.server.authorizations.append(self.headers.get("Authorization"))
        model = body.get("model", "fixture")
        scenario = self.server.scenario
        if scenario == "fallback_after_tool" and model == "backup":
            if self.headers.get("Authorization") != "Bearer backup-fixture":
                self.send_json(400, {"error": "unexpected authorization on backup request"})
                return
        if scenario == "fallback_auth":
            if model == "primary":
                self.send_json(401, {"error": "primary authentication rejected"})
                return
            if self.headers.get("Authorization") is not None:
                self.send_json(400, {"error": "unexpected authorization on keyless route"})
                return
        if self.server.scenario == "disconnect_once" and len(self.server.requests) == 1:
            self.close_connection = True
            return
        tool_failure = (
            self.server.scenario == "tool_retry" and len(self.server.requests) == 2
        ) or (
            self.server.scenario == "fallback_after_tool"
            and body.get("model") == "primary"
            and len(self.server.requests) > 1
        )
        if tool_failure:
            self.send_response(503)
            self.send_header("Retry-After", "0")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.server.scenario == "tool_rate_limit" and len(self.server.requests) == 2:
            data = b'{"error":{"code":"rate_limit_exceeded","message":"Rate limit exceeded"}}'
            self.send_response(429)
            self.send_header("Retry-After", "0")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        if self.server.scenario == "stall":
            time.sleep(30)
            return
        common = {
            "id": "fixture-completion",
            "created": 0,
            "model": model,
        }
        messages = body.get("messages", [])
        if scenario == "stall_on_resume" and messages and messages[-1].get("content") == "hold resume":
            self.server.release.wait(timeout=15)
        if (
            scenario in ("overflow", "overflow_tools")
            and not messages[0]["content"].startswith("Summarize completed conversation history")
            and "Archiveword" in json.dumps(messages)
            and (scenario == "overflow" or any(m.get("role") == "tool" and m.get("tool_call_id") != "preserved" for m in messages))
        ):
            self.send_json(400, {"error": {
                "code": "context_length_exceeded",
                "message": "This model's maximum context length is 8192 tokens.",
            }})
            return
        if scenario == "compact" and messages and messages[-1].get("content") == "Cancel this turn":
            self.server.release.wait(timeout=15)
        function = {"name": "read_file", "arguments": '{"path":"fixture.txt"}'}
        if scenario == "local_tools":
            user_index = max(i for i, message in enumerate(messages) if message.get("role") == "user")
            fixture = json.loads(messages[user_index]["content"])
            step = sum(message.get("role") == "tool" for message in messages[user_index + 1:])
            if fixture.get("read") and step == 0:
                function = {"name": "read_file", "arguments": json.dumps({"path": fixture["path"]})}
            elif "tool" in fixture:
                function = {"name": fixture["tool"], "arguments": json.dumps(fixture["arguments"])}
            else:
                function = {"name": "write_file", "arguments": json.dumps({
                    "path": fixture["path"], "content": fixture["content"],
                })}
            wants_tool = step < (2 if fixture.get("read") else 1)
            call_id = f"file-{step}"
        elif scenario == "memory":
            function = {"name": "memory", "arguments": json.dumps({"target": "user", "operations": [
                {"action": "add", "content": "Prefers Russian replies"}]}, ensure_ascii=False)}
            wants_tool = not any(message.get("role") == "tool" for message in messages)
            call_id = "memory-call"
        elif scenario == "web":
            function = {"name": "web_search", "arguments": '{"query":"hermes agent","limit":2}'}
            wants_tool = not any(message.get("role") == "tool" for message in messages)
            call_id = "web-call"
        elif scenario == "mcp":
            function = {"name": "mcp_fixture_echo", "arguments": '{"text":"from mcp"}'}
            wants_tool = not any(message.get("role") == "tool" for message in messages)
            call_id = "mcp-call"
        elif scenario == "skill":
            last_user = max(i for i, message in enumerate(messages) if message.get("role") == "user")
            wants_tool = not any(message.get("role") == "tool" for message in messages[last_user + 1:])
            call_id = f"skill-{len(self.server.requests)}"
            name = messages[last_user]["content"]
            if name == "list docs skills":
                function = {"name": "skills_list", "arguments": '{"category":"docs"}'}
            else:
                function = {"name": "skill_view", "arguments": json.dumps({"name": name})}
        elif scenario in ("iteration_limit", "iteration_summary_tool"):
            summary = messages and isinstance(messages[-1], dict) and str(messages[-1].get("content", "")).startswith(
                "You've reached the maximum number of tool-calling iterations"
            )
            wants_tool = not summary or scenario == "iteration_summary_tool"
            call_id = f"iteration-{len(self.server.requests)}"
        else:
            wants_tool = scenario in ("tool", "overflow_tools", "partial_tool", "tool_retry", "tool_rate_limit", "fallback_after_tool", "partial_after_tool", "stall_on_resume") and not any(
                isinstance(message, dict) and message.get("role") == "tool"
                for message in messages
            )
            call_id = "fixture-call"
        if wants_tool:
            tool_call = {
                "id": call_id,
                "type": "function",
                "function": function,
            }
            message = {"role": "assistant", "content": None, "tool_calls": [tool_call]}
            finish_reason = "tool_calls"
        else:
            message = {"role": "assistant", "content": "Ответ модели"}
            finish_reason = "stop"
        if scenario in ("compact", "compact_failure", "overflow", "overflow_tools") and messages[0]["content"].startswith("Summarize completed conversation history"):
            message["content"] = "Earlier turns resolved the fixture checks."
            if scenario == "compact_failure":
                finish_reason = "length"
        if not body.get("stream"):
            self.send_json(200, {
                **common,
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": finish_reason,
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })
            return
        delta = dict(message)
        if wants_tool:
            delta["tool_calls"] = [{"index": 0, **tool_call}]
        chunks = [
            {
                **common,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
            },
            {
                **common,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
            },
        ]
        if scenario == "burst":
            chunks = [{**chunks[0], "choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": None}]}] * 256 + chunks[1:]
        data = "".join(f"data: {json.dumps(chunk, ensure_ascii=False)}\n\n" for chunk in chunks)
        if scenario not in ("partial_tool", "partial_text") and not (scenario == "partial_after_tool" and not wants_tool):
            data += "data: [DONE]\n\n"
        data = data.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--tls-cert")
    parser.add_argument("--tls-key")
    parser.add_argument("--scenario", choices=(
        "text", "tool", "partial_tool", "stall", "tool_retry", "tool_rate_limit",
        "fallback_after_tool", "fallback_auth", "partial_text", "disconnect_once",
        "iteration_limit", "iteration_summary_tool", "partial_after_tool", "stall_on_resume", "burst", "skill",
        "compact", "compact_failure", "local_tools", "overflow", "overflow_tools", "mcp", "web", "memory",
    ), default="text")
    args = parser.parse_args()
    if bool(args.tls_cert) != bool(args.tls_key):
        parser.error("--tls-cert and --tls-key must be supplied together")
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.requests = []
    server.authorizations = []
    server.scenario = args.scenario
    server.release = threading.Event()
    try:
        if args.tls_cert:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(args.tls_cert, args.tls_key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
        scheme = "https" if args.tls_cert else "http"
        print(f"{scheme}://127.0.0.1:{server.server_port}", flush=True)
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
