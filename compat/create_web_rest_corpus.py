#!/usr/bin/env python3
"""Record the Keenable and xAI web backends of the Python reference against a loopback recorder.

Usage: create_web_rest_corpus.py <reference checkout> <output.json>
Run it with the reference interpreter. Keenable is reached with `requests` at a hard-coded URL
(the recorder patches the module constant); xAI reads XAI_BASE_URL. Only the keyed paths are
recorded: the keyless ring and the xAI OAuth store need remote endpoints.
"""

import asyncio
import json
import os
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

KEY = "synthetic-key-0000"
U = "http://93.184.216.34/"
KEEN_SEARCH = [
    {"status": 200, "text": json.dumps({"results": [
        {"url": U + "a", "title": "A", "snippet": "alpha", "description": "ignored"},
        {"url": U + "b", "title": None, "description": "beta"},
        {"url": None},
    ]})},
    {"status": 200, "text": json.dumps({"results": None})},
    {"status": 429, "text": "slow down"},
    {"status": 401, "text": ""},
]
KEEN_FETCH = [
    {"status": 200, "text": json.dumps({"url": U + "final", "title": "T", "content": "body"})},
    {"status": 200, "text": json.dumps({"content": None})},
    {"status": 404, "text": json.dumps({"message": "the page does not exist", "code": "x"})},
    {"status": 403, "text": json.dumps({"message": "the target server refused"})},
    {"status": 429, "text": json.dumps({"message": "client rate limit"})},
    {"status": 500, "text": ""},
    {"status": 200, "text": "not json"},
    {"status": 404, "text": json.dumps({"message": "the page is gone"})},
    {"status": 404, "text": json.dumps({"message": "The Target Server said no"})},
    {"status": 404, "text": json.dumps({"message": "the target servers ran"})},
    {"status": 404, "text": json.dumps(["the page does not exist"])},
]


LONG = "".join(chr(97 + i % 26) for i in range(300))


def out_text(payload, annotations=None):
    chunk = {"type": "output_text", "text": payload}
    if annotations is not None:
        chunk["annotations"] = annotations
    return {"output": [{"type": "message", "content": [chunk]}]}


XAI_ANSWERS = [
    {"status": 200, "text": json.dumps(out_text(json.dumps({"results": [
        {"title": " A ", "url": U + "a", "description": "alpha"},
        {"title": "no url", "url": ""},
        {"title": "B", "url": U + "b"},
        5,
    ]})))},
    {"status": 200, "text": json.dumps(out_text('Sure! {"results": [{"title": "P", "url": "' + U + 'p", "description": "prose"}]} done'))},
    {"status": 200, "text": json.dumps(out_text("no json here", [
        {"type": "url_citation", "url": U + "c1", "start_index": 3, "end_index": 6},
        {"type": "url_citation", "url": U + "c1"},
        {"type": "other", "url": U + "c2"},
        {"type": "url_citation", "url": U + "c3", "start_index": 0, "end_index": 2},
    ]))},
    {"status": 200, "text": json.dumps(out_text(LONG, [
        {"type": "url_citation", "url": U + "d1", "start_index": 250, "end_index": 260},
        {"type": "url_citation", "url": U + "d2", "start_index": 100, "end_index": 301},
        {"type": "url_citation", "url": U + "d3", "start_index": 100, "end_index": 300},
        {"type": "url_citation", "url": U + "d4", "start_index": 5, "end_index": 6},
        {"type": "url_citation", "url": U + "d5", "start_index": 6, "end_index": 6},
    ]))},
    {"status": 200, "text": json.dumps(out_text("  pad   text  here", [
        {"type": "url_citation", "url": U + "e1", "start_index": 9, "end_index": 12},
    ]))},
    {"status": 200, "text": json.dumps(out_text(json.dumps({"results": [
        {"title": None, "url": U + "n", "description": 5},
        {"title": True, "url": U + "t", "description": ["x", 1]},
        {"title": "k", "url": 7},
    ]})))},
    {"status": 200, "text": json.dumps({"error": {"message": "m", "code": "c"}})},
    {"status": 200, "text": json.dumps({"error": {"message": "", "code": "c"}})},
    {"status": 400, "text": "e" * 400},
    {"status": 400, "text": "  padded body \n"},
    {"status": 200, "text": json.dumps({"output": [], "citations": [U + "x", "  ", 5, U + "y"]})},
    {"status": 200, "text": json.dumps({"output": []})},
    {"status": 200, "text": json.dumps({"error": {"message": "overloaded"}})},
    {"status": 200, "text": json.dumps({"error": {"code": "refused"}})},
    {"status": 200, "text": json.dumps({"error": {}})},
    {"status": 500, "text": "server broke"},
    {"status": 401, "text": ""},
    {"status": 200, "text": "not json"},
]
XAI_CONFIGS = [
    ("default", ""),
    ("model_and_domains", "web:\n  xai:\n    model: grok-custom\n    allowed_domains: [' a.test ', 'b.test', 5, '']\n"),
    ("excluded", "web:\n  xai:\n    excluded_domains: [x.test]\n"),
    ("both_filters", "web:\n  xai:\n    allowed_domains: [a.test]\n    excluded_domains: [b.test]\n"),
]


def serve(state):
    class Handler(BaseHTTPRequestHandler):
        def handle_any(self, method):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            url = urlparse(self.path)
            auth = (self.headers.get("Authorization") or "").replace(KEY, "<key>")
            state["requests"].append({
                "method": method, "path": url.path, "query": {k: v[0] for k, v in parse_qs(url.query).items()},
                "auth": auth, "title": self.headers.get("X-Keenable-Title"), "agent": self.headers.get("User-Agent") if "x.ai" in self.path or url.path == "/responses" else None,
                "body": json.loads(raw) if raw else None,
            })
            answer = state["answer"]
            data = answer["text"].encode()
            self.send_response(answer["status"])
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_POST(self):
            self.handle_any("POST")

        def do_GET(self):
            self.handle_any("GET")

        def log_message(self, *args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    home = Path(tempfile.mkdtemp())
    os.environ["HERMES_HOME"] = str(home)
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    state = {"requests": [], "answer": None}
    server = serve(state)
    base = f"http://127.0.0.1:{server.server_port}"
    os.environ["KEENABLE_API_KEY"] = KEY
    os.environ["XAI_API_KEY"] = KEY
    os.environ["XAI_BASE_URL"] = base
    import plugins.web.keenable.provider as keenable
    keenable._KEENABLE_API_URL = base
    from plugins.web.xai.provider import XAIWebSearchProvider
    cases = []

    def record(vendor, kind, args, answer, result, extra=None):
        cases.append({"vendor": vendor, "kind": kind, **args, "answer": answer, "requests": state["requests"],
                      "result": result, **(extra or {})})

    provider = keenable.KeenableWebSearchProvider()
    for answer in KEEN_SEARCH:
        for query, limit in [("python asyncio", 5), ("many", 50), ("zero", 0)]:
            state["answer"], state["requests"] = answer, []
            record("keenable", "search", {"query": query, "limit": limit}, answer, provider.search(query, limit))
    urls = [U + "a", U + "b"]
    for answer in KEEN_FETCH:
        state["answer"], state["requests"] = answer, []
        record("keenable", "extract", {"urls": urls}, answer, provider.extract(urls))

    xai = XAIWebSearchProvider()
    for name, config in XAI_CONFIGS:
        (home / "config.yaml").write_text(config, encoding="utf-8")
        for answer in XAI_ANSWERS if name == "default" else XAI_ANSWERS[:1]:
            for query, limit in [("python asyncio", 5), ("many", 500)] if name == "default" else [("q", 3)]:
                state["answer"], state["requests"] = answer, []
                record("xai", "search", {"query": query, "limit": limit, "config": name, "config_text": config}, answer, xai.search(query, limit))
    output.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(len(cases), "cases")


if __name__ == "__main__":
    main()
