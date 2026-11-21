#!/usr/bin/env python3
"""Measure Linux CLI startup and one turn against the loopback model fixture."""

import argparse
import json
import math
import os
import statistics
import subprocess
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
from pathlib import Path
from urllib.request import Request, urlopen

from model_server import Handler


class TimedHandler(Handler):
    def send_response(self, code, message=None):
        self.server.request_time = time.monotonic()
        self.server.received.set()
        if self.server.hold and not self.server.release.wait(timeout=25):
            raise TimeoutError("benchmark did not release the response")
        super().send_response(code, message)

    def do_POST(self):
        try:
            super().do_POST()
        finally:
            self.server.sent.set()


def stats(samples):
    ordered = sorted(samples)
    return {
        "p50": round(statistics.median(ordered), 3),
        "p95": round(ordered[math.ceil(0.95 * len(ordered)) - 1], 3),
    }


def measure(server, base, command, cwd, env, hold=False):
    server.requests.clear()
    server.received.clear()
    server.sent.clear()
    server.release.clear()
    server.hold = hold
    start = time.monotonic()
    with subprocess.Popen(
        command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ) as process:
        try:
            rss = None
            if hold:
                if not server.received.wait(timeout=25):
                    raise RuntimeError("client did not reach the model fixture")
                # Sample both clients at the same point, idle while waiting for the response.
                time.sleep(0.1)
                status = Path(f"/proc/{process.pid}/status").read_text()
                line = next(line for line in status.splitlines() if line.startswith("VmRSS:"))
                rss = int(line.split()[1])
                server.release.set()
            output, error = process.communicate(timeout=25)
            elapsed = (time.monotonic() - start) * 1000
            if process.returncode:
                details = (error or output).decode(errors="replace").strip()
                raise RuntimeError(f"client exited with {process.returncode}: {details}")
            if not server.sent.wait(timeout=5) or len(server.requests) != 1:
                raise RuntimeError(f"expected one model request to {base}")
            if "Ответ модели".encode() not in output:
                raise RuntimeError("client did not print the expected response")
            return elapsed, (server.request_time - start) * 1000, rss
        finally:
            server.release.set()
            if process.poll() is None:
                process.kill()


def benchmark(server, base, startup, turn, samples, env_extra):
    startup_ms, turn_ms, first_request_ms = [], [], []
    idle_kib = None
    for sample in range(samples + 2):
        with tempfile.TemporaryDirectory(prefix="hermes-bench-") as directory:
            profile = Path(directory) / "profile"
            env = {
                "PATH": os.environ["PATH"], "LANG": "C.UTF-8", "HOME": directory,
                "HERMES_HOME": str(profile), **env_extra,
            }
            start = time.monotonic()
            subprocess.run(
                startup, cwd=directory,
                env={**env, "HERMES_HOME": str(Path(directory) / "startup-profile")},
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, check=True, timeout=25,
            )
            startup_elapsed = (time.monotonic() - start) * 1000
            elapsed, first_request, rss = measure(
                server, base, turn(profile), directory, env, hold=sample == samples + 1,
            )
            if sample == samples + 1:
                idle_kib = rss
            elif sample > 0:
                startup_ms.append(startup_elapsed)
                turn_ms.append(elapsed)
                first_request_ms.append(first_request)
    return {
        "startup_help_ms": stats(startup_ms),
        "to_first_model_request_ms": stats(first_request_ms),
        "one_turn_ms": stats(turn_ms),
        "idle_rss_kib": idle_kib,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rust_binary", type=Path)
    parser.add_argument("python_binary", type=Path)
    parser.add_argument("python_checkout", type=Path)
    parser.add_argument("--samples", type=int, default=15)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    # Keep the Python venv entry point. Resolving its symlink would bypass the venv.
    rust = str(args.rust_binary.resolve(strict=True))
    python = str(args.python_binary.absolute())
    checkout = str(args.python_checkout.resolve(strict=True))
    runner = str(Path(__file__).with_name("run_reference_turn.py").resolve())
    with ThreadingHTTPServer(("127.0.0.1", 0), TimedHandler) as server:
        server.scenario = "text"
        server.requests = []
        server.received = threading.Event()
        server.release = threading.Event()
        server.sent = threading.Event()
        server.hold = False
        base = f"http://127.0.0.1:{server.server_port}/v1"
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            # A direct request provides a separate loopback HTTP round-trip baseline.
            request = Request(
                f"{base}/chat/completions",
                data=b'{"model":"fixture","stream":true,"messages":[]}',
                headers={"Content-Type": "application/json"},
            )
            network = []
            for _ in range(args.samples + 1):
                server.sent.clear()
                start = time.monotonic()
                with urlopen(request, timeout=5) as response:
                    response.read()
                network.append((time.monotonic() - start) * 1000)
                if not server.sent.wait(timeout=5):
                    raise RuntimeError("model fixture did not finish the loopback probe")
            results = {
                "samples": args.samples, "loopback_http_ms": stats(network[1:]),
                "rust": benchmark(
                    server, base, [rust, "--help"],
                    lambda _: [rust, "chat", "--oneshot", "-q", "Привет", "--model", "fixture",
                               "--base-url", base],
                    args.samples, {"HERMES_API_KEY": "fixture"},
                ),
                "python": benchmark(
                    server, base, [python, "-m", "hermes_cli.main", "--help"],
                    lambda profile: [python, runner, checkout, str(profile), base],
                    args.samples, {"PYTHONPATH": checkout, "PYTHONNOUSERSITE": "1"},
                ),
            }
            print(json.dumps(results, indent=2))
        finally:
            server.shutdown()
            thread.join()


if __name__ == "__main__":
    main()
