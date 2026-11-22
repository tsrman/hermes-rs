#!/usr/bin/env python3
"""Record the Perplexity web backend of the Python reference against a loopback recorder.

Usage: create_web_backend_corpus.py <reference checkout> <output.json>
Run it with the reference interpreter. Each case names a canned vendor answer; the reference
provider is run against a local server that records the request (key masked), and the case keeps
the request and the provider's return value. The Rust test replays the cases against its own mock.
"""

import json
import os
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

KEY = "pplx-synthetic-key-0000"
SEARCH_ANSWERS = [
    {"status": 200, "body": {"results": [
        {"title": "A", "url": "http://127.0.0.1/x", "snippet": "alpha"},
        {"title": None, "url": "http://127.0.0.1/", "snippet": ""},
        {"url": "http://127.0.0.1/"},
    ]}},
    {"status": 200, "body": {}},
    {"status": 200, "body": {"results": []}},
    {"status": 429, "text": "rate limited"},
    {"status": 401, "text": ""},
]
SEARCH_QUERIES = [("python asyncio", 5), ("q", 1), ("many", 50), ("zero", 0), ("é ü", 3)]
EXTRACT_ANSWERS = [
    {"status": 200, "body": {"results": [
        {"url": "http://127.0.0.1/bloom-filter", "text": "alpha text"},
        {"url": "http://127.0.0.1/2024/x_y", "text": "", "error": "blocked"},
        "junk",
    ]}},
    {"status": 200, "body": {"results": [{"url": "http://127.0.0.1/dup", "text": "one"}, {"url": "http://127.0.0.1/dup", "text": "two"}]}},
    {"status": 200, "body": {}},
    {"status": 500, "text": "boom"},
]
EXTRACT_URLS = [
    ["http://127.0.0.1/bloom-filter", "http://127.0.0.1/2024/x_y", "http://127.0.0.1/missing"],
    ["http://127.0.0.1/dup"],
    ["http://127.0.0.1/"],
    ["http://127.0.0.1/a-b", "http://127.0.0.1/A_B/9"],
]


def serve(state):
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length)
            state["requests"].append({
                "path": self.path,
                "authorization": "Bearer <key>" if self.headers.get("Authorization") == f"Bearer {KEY}" else self.headers.get("Authorization"),
                "body": json.loads(body),
            })
            answer = state["answer"]
            data = (json.dumps(answer["body"]) if "body" in answer else answer["text"]).encode()
            self.send_response(answer["status"])
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    state = {"requests": [], "answer": None}
    server = serve(state)
    os.environ["PERPLEXITY_API_KEY"] = KEY
    os.environ["PERPLEXITY_BASE_URL"] = f"http://127.0.0.1:{server.server_port}"
    from plugins.web.perplexity.provider import PerplexityWebSearchProvider
    provider = PerplexityWebSearchProvider()
    cases = []
    for answer in SEARCH_ANSWERS:
        for query, limit in SEARCH_QUERIES[:2] if answer is not SEARCH_ANSWERS[0] else SEARCH_QUERIES:
            state["answer"], state["requests"] = answer, []
            result = provider.search(query, limit)
            cases.append({"kind": "search", "query": query, "limit": limit, "answer": answer,
                          "requests": state["requests"], "result": result})
    for answer in EXTRACT_ANSWERS:
        for urls in EXTRACT_URLS:
            state["answer"], state["requests"] = answer, []
            result = provider.extract(urls)
            cases.append({"kind": "extract", "urls": urls, "answer": answer,
                          "requests": state["requests"], "result": result})
    os.environ.pop("PERPLEXITY_API_KEY")
    from agent import web_search_provider
    state["requests"] = []
    cases.append({"kind": "search", "query": "nokey", "limit": 5, "answer": None, "requests": [],
                  "result": provider.search("nokey", 5), "no_key": True})
    cases.append({"kind": "extract", "urls": ["http://127.0.0.1/p"], "answer": None, "requests": [],
                  "result": provider.extract(["http://127.0.0.1/p"]), "no_key": True})
    output.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(len(cases), "cases")


if __name__ == "__main__":
    main()
