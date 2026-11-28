import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / ("hsr.exe" if os.name == "nt" else "hsr")


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _http_json(url: str, method: str = "GET", key: str | None = None, payload: dict | None = None):
    headers = {}
    data = None
    if key is not None:
        headers["Authorization"] = f"Bearer {key}"
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode("utf-8")
    req = Request(url, data=data, headers=headers, method=method)
    try:
        with urlopen(req, timeout=15) as resp:
            return resp.status, json.loads(resp.read().decode("utf-8"))
    except HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        try:
            return exc.code, json.loads(body)
        except Exception:
            return exc.code, body


class GatewayCliTest(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.workdir = Path(self.tmpdir.name)
        (self.workdir / "fixture.txt").write_text("fixture content\n", encoding="utf-8")
        self.profile = self.workdir / "profile"
        self.profile.mkdir()

        self.model_proc = subprocess.Popen(
            [sys.executable, str(ROOT / "compat" / "model_server.py"), "--scenario", "tool"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self._stop_proc, self.model_proc)
        self.model_addr = self.model_proc.stdout.readline().strip()
        self.assertTrue(self.model_addr.startswith("http://"), f"unexpected model addr: {self.model_addr!r}")

    def _stop_proc(self, proc: subprocess.Popen):
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        for stream in (proc.stdout, proc.stderr):
            if stream is not None:
                stream.close()

    def test_hsr_gateway_run_serves_runs_api_and_sse_stream(self):
        gw_port = _free_port()
        api_key = "compat-gateway-secret"
        config = {
            "model": {
                "provider": "custom",
                "base_url": f"{self.model_addr}/v1",
                "default": "fixture",
                "api_key": "test-model-key",
            },
            "approvals": {"mode": "off"},
            "agent": {"image_input_mode": "text"},
            "platforms": {
                "api_server": {
                    "enabled": True,
                    "extra": {
                        "host": "127.0.0.1",
                        "port": gw_port,
                        "key": api_key,
                    },
                }
            },
        }
        (self.profile / "config.yaml").write_text(json.dumps(config), encoding="utf-8")

        env = os.environ.copy()
        env["HERMES_HOME"] = str(self.profile)
        gw_proc = subprocess.Popen(
            [str(BINARY), "gateway", "run"],
            cwd=str(self.workdir),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self._stop_proc, gw_proc)

        base_url = f"http://127.0.0.1:{gw_port}"
        deadline = time.monotonic() + 10.0
        ready = False
        while time.monotonic() < deadline:
            if gw_proc.poll() is not None:
                err = gw_proc.stderr.read() if gw_proc.stderr else ""
                self.fail(f"hsr gateway run exited early ({gw_proc.returncode}): {err}")
            try:
                status, body = _http_json(f"{base_url}/health")
                if status == 200 and body.get("status") == "ok":
                    ready = True
                    break
            except Exception:
                time.sleep(0.1)
        self.assertTrue(ready, "gateway /health did not become ready in time")

        # Auth check
        status, _ = _http_json(f"{base_url}/v1/runs", method="POST", payload={"input": "Read fixture.txt"})
        self.assertEqual(status, 401)

        # Start run
        status, run = _http_json(
            f"{base_url}/v1/runs",
            method="POST",
            key=api_key,
            payload={"input": "Read fixture.txt", "session_id": "compat-sess-1"},
        )
        self.assertEqual(status, 200, f"unexpected run create: {run}")
        run_id = run["run_id"]
        self.assertTrue(run_id)

        # Stream SSE events until close
        req = Request(
            f"{base_url}/v1/runs/{run_id}/events",
            headers={"Authorization": f"Bearer {api_key}"},
            method="GET",
        )
        events = []
        with urlopen(req, timeout=20) as resp:
            self.assertEqual(resp.status, 200)
            for raw_line in resp:
                line = raw_line.decode("utf-8", errors="replace").strip()
                if line.startswith("data:"):
                    events.append(json.loads(line[5:].strip()))

        event_names = [e.get("event") for e in events]
        self.assertIn("tool.started", event_names, f"events={events}")
        self.assertIn("tool.completed", event_names, f"events={events}")
        self.assertIn("run.completed", event_names, f"events={events}")

        # Verify final run snapshot
        status, final = _http_json(f"{base_url}/v1/runs/{run_id}", key=api_key)
        self.assertEqual(status, 200)
        self.assertEqual(final["status"], "completed")
        self.assertTrue(final["output"])


if __name__ == "__main__":
    unittest.main()
