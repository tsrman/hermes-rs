#!/usr/bin/env python3
"""Loopback-only OpenAI-compatible server for deterministic parity tests."""

import argparse
import json
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
        if scenario in ("iteration_limit", "iteration_summary_tool"):
            summary = messages and isinstance(messages[-1], dict) and str(messages[-1].get("content", "")).startswith(
                "You've reached the maximum number of tool-calling iterations"
            )
            wants_tool = not summary or scenario == "iteration_summary_tool"
            call_id = f"iteration-{len(self.server.requests)}"
        else:
            wants_tool = scenario in ("tool", "partial_tool", "tool_retry", "tool_rate_limit", "fallback_after_tool") and not any(
                isinstance(message, dict) and message.get("role") == "tool"
                for message in messages
            )
            call_id = "fixture-call"
        if wants_tool:
            tool_call = {
                "id": call_id,
                "type": "function",
                "function": {"name": "read_file", "arguments": '{"path":"fixture.txt"}'},
            }
            message = {"role": "assistant", "content": None, "tool_calls": [tool_call]}
            finish_reason = "tool_calls"
        else:
            message = {"role": "assistant", "content": "Ответ модели"}
            finish_reason = "stop"
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
        data = "".join(f"data: {json.dumps(chunk, ensure_ascii=False)}\n\n" for chunk in chunks)
        if self.server.scenario not in ("partial_tool", "partial_text"):
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
    parser.add_argument("--scenario", choices=(
        "text", "tool", "partial_tool", "stall", "tool_retry", "tool_rate_limit",
        "fallback_after_tool", "fallback_auth", "partial_text", "disconnect_once",
        "iteration_limit", "iteration_summary_tool",
    ), default="text")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.requests = []
    server.scenario = args.scenario
    print(f"http://127.0.0.1:{server.server_port}", flush=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
