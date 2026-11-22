#!/usr/bin/env python3
"""Record the Exa and Parallel web backends of the Python reference against a loopback recorder.

Usage: create_web_sdk_corpus.py <reference checkout> <output.json>
Run it with the reference interpreter with the SDKs of the reference lock installed
(exa-py 2.26.0, parallel-web 0.6.0). Both vendors are reached through their SDKs, so the request
shape (path, auth header, body) is whatever the SDK sends; the recorder keeps it, masked key
included, next to the provider's return value.

Without a key the reference walks a keyless ring of remote MCP endpoints; that path needs the
network and is not recorded (the Rust port refuses those calls with an explicit message instead).
"""

import json
import os
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

KEY = "synthetic-key-0000"
EXA_SEARCH = [
    {"status": 200, "body": {"requestId": "r", "results": [
        {"id": "i1", "url": "http://127.0.0.1/a", "title": "A", "highlights": ["one", "two"]},
        {"id": "i2", "url": "http://127.0.0.1/b", "title": None, "highlights": None},
        {"id": "i3", "url": None, "title": "C"},
    ]}},
    {"status": 200, "body": {"requestId": "r", "results": []}},
    {"status": 401, "body": {"error": "bad key"}},
]
EXA_CONTENTS = [
    {"status": 200, "body": {"requestId": "r", "results": [
        {"id": "http://127.0.0.1/a", "url": "http://127.0.0.1/a", "title": "A", "text": "alpha"},
        {"id": "http://127.0.0.1/b", "url": "http://127.0.0.1/b", "title": None, "text": None},
    ], "statuses": [
        {"id": "http://127.0.0.1/c", "status": "error", "error": {"tag": "CRAWL_HTTP_429", "httpStatusCode": None}},
        {"id": "http://127.0.0.1/d", "status": "error", "error": {"tag": "CRAWL_NOT_FOUND", "httpStatusCode": 404}},
        {"id": "http://127.0.0.1/e", "status": "error", "error": {"tag": "SOURCE_NOT_AVAILABLE", "httpStatusCode": None}},
        {"id": "http://127.0.0.1/f", "status": "error", "error": {"tag": "CRAWL_HTTP_400", "httpStatusCode": None}},
        {"id": "http://127.0.0.1/g", "status": "error", "error": {"tag": "ODD", "httpStatusCode": 399}},
        {"id": "http://127.0.0.1/a", "status": "success"},
    ]}},
    {"status": 500, "body": {"error": "boom"}},
]
PARALLEL_SEARCH = [
    {"status": 200, "body": {"search_id": "s", "results": [
        {"url": "http://127.0.0.1/a", "title": "A", "excerpts": ["one", "two"]},
        {"url": "http://127.0.0.1/b", "title": None, "excerpts": None},
    ]}},
    {"status": 200, "body": {"search_id": "s", "results": []}},
    {"status": 403, "body": {"error": "forbidden"}},
]
PARALLEL_EXTRACT = [
    {"status": 200, "body": {"extract_id": "e", "results": [
        {"url": "http://127.0.0.1/a", "title": "A", "full_content": "alpha", "excerpts": ["x"]},
        {"url": "http://127.0.0.1/b", "title": None, "full_content": None, "excerpts": ["p", "q"]},
    ], "errors": [
        {"url": "http://127.0.0.1/c", "error_type": "http_error", "http_status_code": 404, "content": "gone"},
        {"url": "http://127.0.0.1/d", "error_type": "fetch_failed", "http_status_code": None, "content": None},
        {"url": "http://127.0.0.1/e", "error_type": "http_error", "http_status_code": 503, "content": ""},
        {"url": "http://127.0.0.1/f", "error_type": "http_error", "http_status_code": 400, "content": "bad"},
        {"url": "http://127.0.0.1/g", "error_type": "http_error", "http_status_code": 399, "content": "meh"},
    ]}},
    {"status": 500, "body": {"error": "boom"}},
]
QUERIES = [("python asyncio", 5), ("many", 50), ("one", 1)]
URL_SETS = [["http://127.0.0.1/a", "http://127.0.0.1/b", "http://127.0.0.1/c", "http://127.0.0.1/d", "http://127.0.0.1/e", "http://127.0.0.1/f", "http://127.0.0.1/g"]]
AUTH_HEADERS = ("authorization", "x-api-key")


def serve(state):
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"null")
            auth = {}
            for name in AUTH_HEADERS:
                value = self.headers.get(name)
                if value is not None:
                    auth[name] = value.replace(KEY, "<key>")
            state["requests"].append({"path": self.path, "auth": auth, "body": body})
            answer = state["answer"]
            data = json.dumps(answer["body"]).encode()
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


async def run_all(provider_cls, state, cases, vendor, searches, extracts):
    import asyncio
    provider = provider_cls()
    for answer in searches:
        for query, limit in QUERIES:
            state["answer"], state["requests"] = answer, []
            result = provider.search(query, limit)
            cases.append({"vendor": vendor, "kind": "search", "query": query, "limit": limit, "answer": {**answer, "raw": json.dumps(answer["body"])},
                          "requests": state["requests"], "result": result})
    for answer in extracts:
        for urls in URL_SETS:
            state["answer"], state["requests"] = answer, []
            result = provider.extract(urls)
            if asyncio.iscoroutine(result):
                result = await result
            cases.append({"vendor": vendor, "kind": "extract", "urls": urls, "answer": {**answer, "raw": json.dumps(answer["body"])},
                          "requests": state["requests"], "result": result})
    return provider


def main():
    import asyncio
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    state = {"requests": [], "answer": None}
    server = serve(state)
    base = f"http://127.0.0.1:{server.server_port}"
    os.environ["EXA_API_KEY"] = KEY
    os.environ["PARALLEL_API_KEY"] = KEY
    os.environ["PARALLEL_BASE_URL"] = base
    import exa_py
    real_exa = exa_py.Exa

    def exa_at_recorder(*args, **kwargs):
        client = real_exa(*args, **kwargs)
        client.base_url = base
        return client

    exa_py.Exa = exa_at_recorder
    from plugins.web.exa.provider import ExaWebSearchProvider
    from plugins.web.parallel.provider import ParallelWebSearchProvider
    cases = []
    asyncio.run(run_all(ExaWebSearchProvider, state, cases, "exa", EXA_SEARCH, EXA_CONTENTS))
    asyncio.run(run_all(ParallelWebSearchProvider, state, cases, "parallel", PARALLEL_SEARCH, PARALLEL_EXTRACT))
    output.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(len(cases), "cases")


if __name__ == "__main__":
    main()
