#!/usr/bin/env python3
"""Loopback-only OpenAI-compatible server for deterministic parity tests."""

import argparse
import json
import ssl
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# What the `reply_secrets` scenario says, with a made-up phone number and key.
REPLY_SECRETS = "Calling +15551234567 with key sk-abcdefghijklmnop1234567890 now."

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
        elif self.path == "/headers":
            self.send_json(200, getattr(self.server, "headers_seen", []))
        elif self.path == "/authorizations":
            self.send_json(200, getattr(self.server, "authorizations", []))
        elif self.path == "/release" and self.server.scenario in ("stall_on_resume", "compact"):
            self.server.release.set()
            self.send_json(200, {})
        else:
            self.send_json(404, {"error": "unknown endpoint"})

    def do_POST(self):
        if self.path == "/v1/responses":
            # No Responses scenario exists: record the request for tests that look at its body, then refuse.
            try:
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            except (ValueError, UnicodeDecodeError):
                body = None
            if isinstance(body, dict):
                self.server.requests.append(body)
            self.send_json(404, {"error": "responses are only recorded"})
            return
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
        if not hasattr(self.server, "headers_seen"):
            self.server.headers_seen = []
        self.server.headers_seen.append({key.lower(): value for key, value in self.headers.items()})
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
        summarizing = bool(messages) and str(messages[0].get("content", "")).startswith("Summarize completed conversation history")
        raw_tools = sum(m.get("role") == "tool" for m in messages)
        if (
            scenario == "overflow_midturn"
            and not summarizing
            and "[CONTEXT SUMMARY]" not in json.dumps(messages)
            and raw_tools >= 3
        ):
            self.send_json(400, {"error": {
                "code": "context_length_exceeded",
                "message": "This model's maximum context length is 8192 tokens.",
            }})
            return
        if scenario == "compact" and messages and messages[-1].get("content") == "Cancel this turn":
            self.server.release.wait(timeout=15)
        function = {"name": "read_file", "arguments": '{"path":"fixture.txt"}'}
        final_text = None
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
        elif scenario == "todo":
            last_user = max(i for i, message in enumerate(messages) if message.get("role") == "user")
            wants_tool = not any(message.get("role") == "tool" for message in messages[last_user + 1:])
            call_id = f"todo-{len(self.server.requests)}"
            if messages[last_user]["content"] == "write":
                function = {"name": "todo_list", "arguments": json.dumps({"todos": [
                    {"id": "1", "content": "Draft the plan", "status": "in_progress"},
                    {"id": "2", "content": "Review it", "status": "pending", "parent": "1"}]})}
            else:
                function = {"name": "todo_list", "arguments": "{}"}
        elif scenario == "delegate":
            # The parent delegates two tasks, then checks its own directory; a child told to
            # MOVE changes directory, the other just answers. Children are told apart by prompt.
            child = str(messages[0].get("content", "")).startswith("You are a focused subagent")
            tools_seen = sum(message.get("role") == "tool" for message in messages)
            if child:
                goal = str(messages[-1 if messages[-1].get("role") == "user" else 1].get("content", ""))
                moves = "MOVE" in str(messages[1].get("content", ""))
                wants_tool = moves and tools_seen == 0
                function = {"name": "terminal", "arguments": json.dumps({"command": "mkdir -p moved && cd moved && pwd; rm -rf ./no-such-dir-for-approval"})}
                call_id = "child-move"
                if not wants_tool:
                    marker = str(messages[1].get("content", "")).split("TASK:\n")[-1][:14]
                    final_text = f"child-done:{marker}"
            else:
                wants_tool = tools_seen < 2
                if tools_seen == 0:
                    function = {"name": "delegate_task", "arguments": json.dumps({"tasks": [
                        {"goal": "MOVE around the project", "context": "ctx A"},
                        {"goal": "Just answer quickly please"}]})}
                    call_id = "deleg-0"
                else:
                    function = {"name": "terminal", "arguments": json.dumps({"command": "pwd"})}
                    call_id = "parent-pwd"
        elif scenario == "code":
            # The user message is JSON {"cells": [python source, ...]}; one execute_code call each.
            user_index = max(i for i, message in enumerate(messages) if message.get("role") == "user")
            cells = json.loads(messages[user_index]["content"])["cells"]
            step = sum(message.get("role") == "tool" for message in messages[user_index + 1:])
            wants_tool = step < len(cells)
            function = {"name": "execute_code", "arguments": json.dumps({"code": cells[step]} if wants_tool else {})}
            call_id = f"code-{step}"
        elif scenario == "cron_failure":
            wants_tool = False
            call_id = "none"
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
            function = {"name": "mcp__fixture__echo", "arguments": '{"text":"from mcp"}'}
            wants_tool = not any(message.get("role") == "tool" for message in messages)
            call_id = "mcp-call"
        elif scenario == "mcp_resources":
            steps = sum(message.get("role") == "tool" for message in messages)
            wants_tool = steps < 2
            call_id = f"mcp-resource-{steps}"
            function = ({"name": "mcp__fixture__list_resources", "arguments": "{}"} if steps == 0 else
                        {"name": "mcp__fixture__read_resource", "arguments": '{"uri":"mem://plain"}'})
        elif scenario == "mcp_old_name":
            # The name this port gave MCP tools before it followed the reference (`mcp__<server>__<tool>`).
            last_user = max(i for i, message in enumerate(messages) if message.get("role") == "user")
            wants_tool = not any(message.get("role") == "tool" for message in messages[last_user + 1:])
            call_id = f"old-name-{len(self.server.requests)}"
            function = {"name": "mcp_fixture_echo", "arguments": '{"text":"old"}'}
        elif scenario == "mcp_slow":
            function = {"name": "mcp__fixture__slow", "arguments": "{}"}
            wants_tool = not any(message.get("role") == "tool" for message in messages)
            call_id = "mcp-slow-call"
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
            wants_tool = scenario in ("tool", "reply_secrets", "overflow_tools", "partial_tool", "tool_retry", "tool_rate_limit", "fallback_after_tool", "partial_after_tool", "stall_on_resume") and not any(
                isinstance(message, dict) and message.get("role") == "tool"
                for message in messages
            )
            call_id = "fixture-call"
            if scenario in ("overflow_midturn", "pressure_midturn"):
                wants_tool = not summarizing and raw_tools < 3 and "[CONTEXT SUMMARY]" not in json.dumps(messages)
                call_id = f"midturn-call-{len(self.server.requests)}"
            if scenario in ("pressure_long", "pressure_summary_fail", "pressure_summary_grow"):
                # Tool calls until ten requests have been made, however much was compressed meanwhile.
                wants_tool = not summarizing and len(self.server.requests) <= 10
                call_id = f"long-call-{len(self.server.requests)}"
        if wants_tool:
            tool_call = {
                "id": call_id,
                "type": "function",
                "function": function,
            }
            message = {"role": "assistant", "content": REPLY_SECRETS if scenario == "reply_secrets" else None,
                       "tool_calls": [tool_call]}
            finish_reason = "tool_calls"
        else:
            message = {"role": "assistant", "content": (
                "[CRON_FAILURE]\nchild task failed" if scenario == "cron_failure" else (
                    REPLY_SECRETS if scenario == "reply_secrets" else (final_text or "Ответ модели")))}
            finish_reason = "stop"
        if scenario in ("compact", "compact_failure", "overflow", "overflow_tools", "overflow_midturn", "pressure_midturn", "pressure_long", "pressure_summary_fail", "pressure_summary_grow") and messages[0]["content"].startswith("Summarize completed conversation history"):
            message["content"] = "Earlier turns resolved the fixture checks."
            if scenario in ("compact_failure", "pressure_summary_fail"):
                finish_reason = "length"
            elif scenario == "pressure_summary_grow":
                message["content"] = "Oversized summary " + ("x" * 20000)
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
        "compact", "compact_failure", "local_tools", "overflow", "overflow_tools", "overflow_midturn", "pressure_midturn", "mcp", "mcp_resources", "mcp_old_name", "mcp_slow", "web", "memory", "cron_failure", "todo", "delegate", "code", "reply_secrets", "pressure_long", "pressure_summary_fail", "pressure_summary_grow",
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
