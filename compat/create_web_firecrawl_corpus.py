#!/usr/bin/env python3
"""Record the Firecrawl web backend (keyed or self-hosted, SDK path) of the Python reference.

Usage: create_web_firecrawl_corpus.py <reference checkout> <output.json>
Run it with the reference interpreter with firecrawl-py of the reference lock (4.53.2) installed.
FIRECRAWL_API_URL points the SDK at a loopback recorder; the cases keep the request (path, auth
header masked, body) and the provider's return value. Keyless cloud mode and the managed gateway
need remote endpoints and are not recorded.
"""

import asyncio
import json
import os
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

KEY = "fc-synthetic-key-0000"
U = "http://93.184.216.34/"
SEARCH_ANSWERS = [
    {"status": 200, "body": {"success": True, "data": {"web": [
        {"url": U + "a", "title": "A", "description": "alpha", "category": "x", "position": 1},
        {"url": U + "b", "title": None, "description": None},
        {"url": U + "c"},
    ]}}},
    {"status": 200, "body": {"success": True, "data": {"web": []}}},
    {"status": 200, "body": {"success": True, "data": [{"url": U + "l", "title": "L", "description": "list"}]}},
    {"status": 402, "body": {"success": False, "error": "Payment required"}},
    {"status": 500, "body": {"success": False, "error": "boom"}},
    {"status": 400, "body": {"success": False, "error": "bad query", "details": "limit too big"}},
    {"status": 401, "body": {"success": False, "error": "Unauthorized"}},
    {"status": 403, "body": {"success": False, "error": "nope", "details": "why"}},
    {"status": 404, "body": {"success": False}},
    {"status": 408, "body": {"success": False, "error": "slow"}},
    {"status": 502, "text": "<html>bad gateway</html>"},
    {"status": 500, "text": ""},
    {"status": 200, "body": {"success": False, "error": "soft failure"}},
]
SCRAPE_ANSWERS = [
    {"status": 200, "body": {"success": True, "data": {
        "markdown": "# alpha", "html": "<h1>alpha</h1>",
        "metadata": {"title": "A", "sourceURL": U + "a", "url": U + "a", "statusCode": 200}}}},
    {"status": 200, "body": {"success": True, "data": {
        "markdown": "", "html": "<p>only html</p>",
        "metadata": {"title": "H", "sourceURL": U + "a", "url": U + "a", "statusCode": 200}}}},
    {"status": 200, "body": {"success": True, "data": {
        "markdown": "gone", "html": None,
        "metadata": {"title": "", "sourceURL": U + "a", "url": U + "a", "statusCode": 404, "error": "Not Found"}}}},
    {"status": 200, "body": {"success": True, "data": {
        "markdown": "moved", "metadata": {"title": "R", "sourceURL": U + "a", "url": U + "final", "statusCode": 200}}}},
    {"status": 200, "body": {"success": True, "data": {
        "markdown": "x", "metadata": {"title": "P", "sourceURL": U + "a", "url": "http://10.0.0.5/secret", "statusCode": 200}}}},
    {"status": 200, "body": {"success": True, "data": {"markdown": "bare"}}},
    {"status": 429, "body": {"success": False, "error": "Rate limit exceeded"}},
    {"status": 500, "body": {"success": False, "error": "boom"}},
    {"status": 401, "body": {"success": False, "error": "Unauthorized", "details": "bad key"}},
    {"status": 403, "body": {"success": False, "error": "blocked site"}},
    {"status": 408, "body": {"success": False, "error": "slow"}},
    {"status": 502, "text": "<html>bad gateway</html>"},
    {"status": 200, "body": {"success": False, "error": "soft failure"}},
]
FORMATS = [None]  # web_extract has no format parameter


def record(answer):
    raw = json.dumps(answer["body"]) if "body" in answer else answer["text"]
    return {"status": answer["status"], "raw": raw}


def serve(state):
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"null")
            auth = (self.headers.get("Authorization") or "").replace(KEY, "<key>")
            state["requests"].append({"path": self.path, "auth": auth, "body": body})
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
    os.environ["FIRECRAWL_API_URL"] = f"http://127.0.0.1:{server.server_port}"
    from plugins.web.firecrawl.provider import FirecrawlWebSearchProvider
    provider = FirecrawlWebSearchProvider()
    cases = []
    for mode in ("keyed", "self_hosted"):
        if mode == "keyed":
            os.environ["FIRECRAWL_API_KEY"] = KEY
        else:
            os.environ.pop("FIRECRAWL_API_KEY", None)
        import tools.web_tools as web_tools
        web_tools._firecrawl_client = None
        # The keyless variants and the full answer matrix are recorded in keyed mode; a self-hosted
        # server (URL only) needs just the happy paths to show the Authorization header is absent.
        searches = SEARCH_ANSWERS if mode == "keyed" else SEARCH_ANSWERS[:1]
        scrapes = SCRAPE_ANSWERS if mode == "keyed" else SCRAPE_ANSWERS[:1]
        for answer in searches:
            for query, limit in [("python asyncio", 5), ("one", 1)]:
                state["answer"], state["requests"] = answer, []
                result = provider.search(query, limit)
                cases.append({"mode": mode, "kind": "search", "query": query, "limit": limit, "answer": record(answer),
                              "requests": state["requests"], "result": result})
        for answer in scrapes:
            for fmt in FORMATS:
                state["answer"], state["requests"] = answer, []
                result = asyncio.run(provider.extract([U + "a"], format=fmt))
                cases.append({"mode": mode, "kind": "extract", "format": fmt, "urls": [U + "a"], "answer": record(answer),
                              "requests": state["requests"], "result": result})
    output.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(len(cases), "cases")


if __name__ == "__main__":
    main()
