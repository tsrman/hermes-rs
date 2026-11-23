"""End-to-end CLI tests for NVIDIA, AWS Bedrock Converse, Google Vertex AI, and Codex OAuth."""

import base64
import json
import os
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import parse_qs

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / ("hermes.exe" if os.name == "nt" else "hermes")


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _make_jwt(payload: dict) -> str:
    header = _b64url(json.dumps({"alg": "none"}).encode("utf-8"))
    body = _b64url(json.dumps(payload).encode("utf-8"))
    return f"{header}.{body}.sig"


def _start_server(handler_cls):
    server = HTTPServer(("127.0.0.1", 0), handler_cls)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, f"http://127.0.0.1:{server.server_port}"


def _close_server(server):
    server.shutdown()
    server.server_close()


class ProvidersRemainderCliTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.workdir = Path(self.tmp.name)
        self.home = self.workdir / "hermes"
        self.home.mkdir()

    def _run_chat(self, query: str, extra_env: dict, extra_args=()):
        env = {
            "PATH": os.environ.get("PATH", ""),
            "HERMES_HOME": str(self.home),
            **extra_env,
        }
        return subprocess.run(
            [str(BINARY), "chat", "--oneshot", "-q", query, *extra_args],
            cwd=self.workdir,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def test_nvidia_provider_strips_tool_message_names_end_to_end(self):
        (self.workdir / "note.txt").write_text("hello nvidia", encoding="utf-8")
        recorded = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length))
                recorded.append((self.path, {k.lower(): v for k, v in self.headers.items()}, body))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                if len(recorded) == 1:
                    chunks = [
                        {
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {
                                        "tool_calls": [
                                            {
                                                "index": 0,
                                                "id": "call_nv_1",
                                                "type": "function",
                                                "function": {
                                                    "name": "read_file",
                                                    "arguments": json.dumps({"path": "note.txt"}),
                                                },
                                            }
                                        ]
                                    },
                                    "finish_reason": None,
                                }
                            ]
                        },
                        {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                    ]
                else:
                    chunks = [
                        {"choices": [{"index": 0, "delta": {"content": "Read ok"}, "finish_reason": "stop"}]}
                    ]
                for chunk in chunks:
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode("utf-8"))
                self.wfile.write(b"data: [DONE]\n\n")

        server, base = _start_server(Handler)
        self.addCleanup(_close_server, server)
        (self.home / "config.yaml").write_text(
            "model:\n  provider: nvidia-nim\n  default: nvidia/nemotron-4\n",
            encoding="utf-8",
        )
        res = self._run_chat(
            "Read note.txt",
            {"NVIDIA_API_KEY": "nv-secret", "NVIDIA_BASE_URL": f"{base}/v1"},
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout, "Read ok\n")
        self.assertEqual(len(recorded), 2)
        self.assertEqual(recorded[0][1].get("authorization"), "Bearer nv-secret")
        tool_msgs = [m for m in recorded[1][2]["messages"] if m.get("role") == "tool"]
        self.assertEqual(len(tool_msgs), 1)
        self.assertNotIn("name", tool_msgs[0])
        self.assertNotIn("tool_name", tool_msgs[0])
        self.assertIn("hello nvidia", tool_msgs[0]["content"])

    def test_bedrock_converse_sigv4_and_guardrail_end_to_end(self):
        (self.workdir / "data.txt").write_text("bedrock-ok", encoding="utf-8")
        recorded = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length))
                recorded.append((self.path, {k.lower(): v for k, v in self.headers.items()}, body))
                if len(recorded) == 1:
                    reply = {
                        "output": {
                            "message": {
                                "role": "assistant",
                                "content": [
                                    {
                                        "toolUse": {
                                            "toolUseId": "tu_br_1",
                                            "name": "read_file",
                                            "input": {"path": "data.txt"},
                                        }
                                    }
                                ],
                            }
                        },
                        "stopReason": "tool_use",
                        "usage": {"inputTokens": 20, "outputTokens": 10},
                    }
                else:
                    reply = {
                        "output": {
                            "message": {
                                "role": "assistant",
                                "content": [{"text": "Bedrock reply"}],
                            }
                        },
                        "stopReason": "end_turn",
                        "usage": {"inputTokens": 30, "outputTokens": 5},
                    }
                payload = json.dumps(reply).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        server, base = _start_server(Handler)
        self.addCleanup(_close_server, server)
        (self.home / "config.yaml").write_text(
            "model:\n"
            "  provider: bedrock\n"
            "  default: anthropic.claude-sonnet-4-20250514-v1:0\n"
            "bedrock:\n"
            "  region: us-west-2\n"
            "  guardrail:\n"
            "    id: gr-live\n"
            "    version: '3'\n",
            encoding="utf-8",
        )
        res = self._run_chat(
            "Read data.txt",
            {
                "BEDROCK_BASE_URL": base,
                "AWS_ACCESS_KEY_ID": "AKIAEXAMPLE123",
                "AWS_SECRET_ACCESS_KEY": "secretkeyexample456",
            },
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout, "Bedrock reply\n")
        self.assertEqual(len(recorded), 2)
        self.assertEqual(
            recorded[0][0],
            "/model/anthropic.claude-sonnet-4-20250514-v1%3A0/converse",
        )
        auth = recorded[0][1].get("authorization", "")
        self.assertTrue(auth.startswith("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE123/"), auth)
        self.assertIn("/us-west-2/bedrock/aws4_request", auth)
        self.assertEqual(
            recorded[0][2].get("guardrailConfig"),
            {"guardrailIdentifier": "gr-live", "guardrailVersion": "3"},
        )
        # Second turn includes toolResult block from read_file
        tool_result_msg = recorded[1][2]["messages"][-1]
        self.assertEqual(tool_result_msg["role"], "user")
        self.assertIn("bedrock-ok", tool_result_msg["content"][0]["toolResult"]["content"][0]["text"])

    def test_vertex_provider_uses_access_token_and_base_url_override(self):
        recorded = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length))
                recorded.append((self.path, {k.lower(): v for k, v in self.headers.items()}, body))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                chunk = {"choices": [{"index": 0, "delta": {"content": "Vertex ok"}, "finish_reason": "stop"}]}
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode("utf-8"))
                self.wfile.write(b"data: [DONE]\n\n")

        server, base = _start_server(Handler)
        self.addCleanup(_close_server, server)
        (self.home / "config.yaml").write_text(
            "model:\n  provider: vertex-ai\n  default: google/gemini-2.5-pro\n",
            encoding="utf-8",
        )
        res = self._run_chat(
            "hi vertex",
            {
                "VERTEX_ACCESS_TOKEN": "ya29.test_vertex_tok",
                "VERTEX_BASE_URL": base,
            },
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout, "Vertex ok\n")
        self.assertEqual(len(recorded), 1)
        self.assertEqual(recorded[0][1].get("authorization"), "Bearer ya29.test_vertex_tok")

    def test_codex_oauth_refreshes_expiring_token_and_calls_responses_api(self):
        old_jwt = _make_jwt({"exp": 1000, "sub": "u1"})
        new_jwt = _make_jwt({"exp": 4102444800, "sub": "u1"})
        token_requests = []
        api_requests = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length).decode("utf-8")
                if self.path == "/oauth/token":
                    token_requests.append(parse_qs(raw))
                    payload = json.dumps(
                        {"access_token": new_jwt, "refresh_token": "rt_new"}
                    ).encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                else:
                    api_requests.append(
                        (self.path, {k.lower(): v for k, v in self.headers.items()}, json.loads(raw))
                    )
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    for ev in [
                        {"type": "response.output_text.delta", "delta": "Codex ok"},
                        {"type": "response.completed"},
                    ]:
                        self.wfile.write(f"data: {json.dumps(ev)}\n\n".encode("utf-8"))

        server, base = _start_server(Handler)
        self.addCleanup(_close_server, server)
        (self.home / "config.yaml").write_text(
            "model:\n  provider: openai-codex\n  default: gpt-5.4\n",
            encoding="utf-8",
        )
        (self.home / "auth.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "providers": {
                        "openai-codex": {
                            "tokens": {
                                "access_token": old_jwt,
                                "refresh_token": "rt_old",
                            }
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        res = self._run_chat(
            "hi codex",
            {
                "HERMES_CODEX_TOKEN_URL": f"{base}/oauth/token",
                "HERMES_CODEX_BASE_URL": base,
            },
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout, "Codex ok\n")
        self.assertEqual(len(token_requests), 1)
        self.assertEqual(token_requests[0]["refresh_token"], ["rt_old"])
        self.assertEqual(len(api_requests), 1)
        self.assertEqual(api_requests[0][0], "/responses")
        self.assertEqual(api_requests[0][1].get("authorization"), f"Bearer {new_jwt}")

        persisted = json.loads((self.home / "auth.json").read_text(encoding="utf-8"))
        self.assertEqual(
            persisted["providers"]["openai-codex"]["tokens"]["refresh_token"],
            "rt_new",
        )


if __name__ == "__main__":
    unittest.main()
