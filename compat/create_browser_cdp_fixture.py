#!/usr/bin/env python3
"""Record browser_cdp behaviour from the Python reference.

Usage: create_browser_cdp_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter (the `websockets` package must be installed in it). The
output holds URL-literal and metadata-floor cases, result-redaction cases, and end-to-end calls:
each runs the reference tool against a scripted fake CDP server and records the tool's answer and
every frame the server received. `browser_cdp.rs` replays the same scripts on its own fake server.
Only IP literals are used for hosts, so nothing depends on DNS.
"""

import json
import os
import sys
import tempfile
import threading
from pathlib import Path

SECRET = "password=hunter2hunter2xyz"
FERNET = "gAAAA" + "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo" * 2
PUBLIC_PAGE = {"targetId": "T1", "type": "page", "url": "https://93.184.216.34/home", "title": "Home"}
PRIVATE_PAGE = {"targetId": "T2", "type": "page", "url": "http://10.0.0.5/admin", "title": "Admin"}
WORKER = {"targetId": "W1", "type": "service_worker", "url": "https://93.184.216.34/sw.js", "title": ""}
BLANK = {"targetId": "B1", "type": "page", "url": "about:blank", "title": ""}

LITERAL_TEXTS = [
    "fetch('http://10.0.0.1/a?b=//c') + '//evil.test/x'",
    "a//b // note",
    "go http://x)@10.0.0.2/ and more",
    "see https://example.test/path?next=//other.test/x.",
    "http://[::1]:8080/ and http://[fd00:ec2::254]/latest",
    r"\\host\share and //[::1]/x and ///triple.test",
    "https://user:pw@192.168.0.1/x, http://a.test/b;",
    "x = `http://172.16.0.1/${id}` + \"https://8.8.8.8/\"",
    "no urls here",
    "HTTP://UPPER.test/Path",
    "http://x@y@10.0.0.3/",
    "(http://10.0.0.4)",
]
METADATA_LITERALS = [
    "http://169.254.169.254/latest/meta-data",
    "http://[::ffff:169.254.169.254]/",
    "http://0xa9fea9fe/",
    "http://metadata.google.internal/computeMetadata",
    "http://10.0.0.1/",
    "https://93.184.216.34/",
    "http://169.254.169.%32%35%34/",
    "//169.254.169.254/x",
    "\\\\169.254.169.254\\x",
    "//0xa9fea9fe/x",
    "//2852039166/x",
    "//0251.0376.0251.0376/",
    "//169.16689662/",
    "//metadata.goog/",
    "//METADATA.GOOGLE.INTERNAL./x",
    "//10.0.0.1/x",
    "//93.184.216.34/x",
    "//[fd00:ec2::254]/x",
    "//169.254.169.254。/x",
    "//%31%36%39.254.169.254/x",
    "//?x",
    "//[::1/x",
    "//user@169.254.169.254/x",
    "//169.254.169.254:80/x",
    "/etc/passwd",
]


def nested(depth):
    value = "leaf " + SECRET
    for _ in range(depth):
        value = {"k": value}
    return value


REDACT_CASES = [
    ("Runtime.evaluate", {"result": {"type": "string", "value": f"token {SECRET}"}}),
    ("Page.captureScreenshot", {"data": FERNET}),
    ("Page.captureScreenshot", {"data": FERNET, "other": FERNET}),
    ("Page.printToPDF", {"data": FERNET, "stream": SECRET}),
    ("Network.getResponseBody", {"body": FERNET, "base64Encoded": True}),
    ("Network.getResponseBody", {"body": FERNET, "base64Encoded": False}),
    ("Network.getResponseBody", {"body": FERNET, "base64Encoded": "true"}),
    ("Network.getResponseBody", {"body": FERNET}),
    ("IO.read", {"data": FERNET, "base64Encoded": True, "eof": False}),
    ("Network.getRequestPostData", {"postData": FERNET, "base64Encoded": True}),
    ("CacheStorage.requestCachedResponse", {"response": {"body": FERNET, "x": SECRET}}),
    ("CacheStorage.requestCachedResponse", {"body": FERNET}),
    ("Runtime.evaluate", {"result": {"value": {"data": FERNET, "base64Encoded": True}}}),
    ("Page.captureScreenshot", {"nested": {"data": FERNET}}),
    ("Page.captureScreenshot", {"data": [FERNET]}),
    ("Page.captureScreenshot", {"data": 5}),
    ("Runtime.evaluate", {SECRET: 1, "list": [SECRET, {"a": SECRET}], "n": None, "f": 1.5, "t": True}),
    ("Runtime.evaluate", nested(10)),
    ("Runtime.evaluate", nested(318)),
    ("Runtime.evaluate", nested(319)),
    ("Runtime.evaluate", nested(400)),
    ("Runtime.evaluate", {"a": [[[[SECRET]]]]}),
]

# name, arguments, targets, script (method -> reply), config
CALLS = [
    ("list_tabs", {"method": "Target.getTargets"}, [PUBLIC_PAGE, PRIVATE_PAGE], {}, {}),
    ("browser_version", {"method": "Browser.getVersion", "params": {}}, [PRIVATE_PAGE],
     {"Browser.getVersion": {"result": {"product": f"Chrome/1 {SECRET}"}}}, {}),
    ("evaluate_on_public_page",
     {"method": "Runtime.evaluate", "params": {"expression": "1+1", "returnByValue": True}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"type": "number", "value": 2}}}}, {}),
    ("evaluate_on_private_page",
     {"method": "Runtime.evaluate", "params": {"expression": "document.title"}, "target_id": "T2"},
     [PUBLIC_PAGE, PRIVATE_PAGE], {}, {}),
    ("evaluate_without_target_while_any_page_private",
     {"method": "Runtime.evaluate", "params": {"expression": "1"}},
     [PUBLIC_PAGE, PRIVATE_PAGE], {}, {}),
    ("evaluate_on_public_target_with_private_sibling",
     {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE, PRIVATE_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": 1}}}}, {}),
    ("evaluate_on_worker_with_private_page",
     {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "W1"},
     [WORKER, PRIVATE_PAGE], {}, {}),
    ("evaluate_on_worker_with_public_page",
     {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "W1"},
     [WORKER, PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": 1}}}}, {}),
    ("originless_page_does_not_lock",
     {"method": "Runtime.evaluate", "params": {"expression": "1"}},
     [BLANK, PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": 1}}}}, {}),
    ("unreadable_targets",
     {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Target.getTargets": {"error": {"code": -32000, "message": "nope"}}}, {}),
    ("navigate_to_loopback", {"method": "Page.navigate", "params": {"url": "http://127.0.0.1:8000/"}},
     [PUBLIC_PAGE], {}, {}),
    ("navigate_to_metadata", {"method": "Page.navigate", "params": {"url": "http://169.254.169.254/latest"}},
     [PUBLIC_PAGE], {}, {}),
    ("navigate_to_metadata_private_allowed",
     {"method": "Page.navigate", "params": {"url": "http://169.254.169.254/latest"}},
     [PUBLIC_PAGE], {}, {"allow_private_urls": True}),
    ("navigate_to_public", {"method": "Page.navigate", "params": {"url": "https://93.184.216.34/"}},
     [PRIVATE_PAGE], {"Page.navigate": {"result": {"frameId": "F1"}}}, {}),
    ("navigate_to_file", {"method": "Page.navigate", "params": {"url": "file:///etc/passwd"}},
     [PUBLIC_PAGE], {}, {}),
    ("create_blank_tab", {"method": "Target.createTarget", "params": {"url": "about:blank"}},
     [PRIVATE_PAGE], {"Target.createTarget": {"result": {"targetId": "N1"}}}, {}),
    ("create_private_tab", {"method": "Target.createTarget", "params": {"url": "http://10.1.1.1/"}},
     [PUBLIC_PAGE], {}, {}),
    ("evaluate_fetches_private", {"method": "Runtime.evaluate",
                                  "params": {"expression": "fetch('http://192.168.1.1/x')"}, "target_id": "T1"},
     [PUBLIC_PAGE], {}, {}),
    ("evaluate_scheme_relative_metadata", {"method": "Runtime.evaluate",
                                          "params": {"expression": "fetch('//169.254.169.254/x')"},
                                          "target_id": "T1"},
     [PUBLIC_PAGE], {}, {}),
    ("evaluate_metadata_private_allowed", {"method": "Runtime.evaluate",
                                           "params": {"expression": "fetch('http://169.254.169.254/x')"},
                                           "target_id": "T1"},
     [PUBLIC_PAGE], {}, {"allow_private_urls": True}),
    ("evaluate_private_private_allowed", {"method": "Runtime.evaluate",
                                          "params": {"expression": "fetch('http://10.0.0.9/x')"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": 1}}}}, {"allow_private_urls": True}),
    ("load_network_resource_private", {"method": "Network.loadNetworkResource",
                                       "params": {"url": "http://10.1.1.1/x", "frameId": "F"}, "target_id": "T1"},
     [PUBLIC_PAGE], {}, {}),
    ("call_function_body_metadata", {"method": "Runtime.callFunctionOn",
                                     "params": {"functionDeclaration": "() => fetch('http://169.254.169.254/')"},
                                     "target_id": "T1"},
     [PUBLIC_PAGE], {}, {}),
    ("blocked_url_patterns_not_fetched", {"method": "Network.setBlockedURLs",
                                          "params": {"urls": ["http://10.1.1.1/*"]}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Network.setBlockedURLs": {"result": {}}}, {}),
    ("input_text_not_fetched", {"method": "Input.insertText",
                                "params": {"text": "http://169.254.169.254/"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Input.insertText": {"result": {}}}, {}),
    ("cookies_scope_not_fetched", {"method": "Network.setCookie",
                                   "params": {"name": "a", "value": "b", "url": "http://10.1.1.1/"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Network.setCookie": {"result": {"success": True}}}, {}),
    ("screenshot_stays_opaque", {"method": "Page.captureScreenshot", "target_id": "T1"},
     [PUBLIC_PAGE], {"Page.captureScreenshot": {"result": {"data": FERNET}}}, {}),
    ("response_body_flagged", {"method": "Network.getResponseBody", "params": {"requestId": "R"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Network.getResponseBody": {"result": {"body": FERNET, "base64Encoded": True}}}, {}),
    ("response_body_text", {"method": "Network.getResponseBody", "params": {"requestId": "R"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Network.getResponseBody": {"result": {"body": f"x {SECRET}", "base64Encoded": False}}}, {}),
    ("events_and_stray_replies_are_ignored", {"method": "Browser.getVersion"}, [PUBLIC_PAGE],
     {"Browser.getVersion": {"result": {"product": "Chrome"}, "noise": True}}, {}),
    ("cdp_error", {"method": "Foo.bar", "params": {"a": 1}, "target_id": "T1"}, [PUBLIC_PAGE],
     {"Foo.bar": {"error": {"code": -32601, "message": "'Foo.bar' wasn't found"}}}, {}),
    ("cdp_error_without_target", {"method": "Browser.getVersion"}, [PUBLIC_PAGE],
     {"Browser.getVersion": {"error": {"code": -1, "message": "boom", "data": None}}}, {}),
    ("attach_fails", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Target.attachToTarget": {"error": {"code": -32602, "message": "No target"}}}, {}),
    ("attach_without_session", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Target.attachToTarget": {"result": {}}}, {}),
    ("missing_result_is_empty", {"method": "Page.reload"}, [PUBLIC_PAGE], {"Page.reload": {"raw": {}}}, {}),
    ("timeout", {"method": "Browser.getVersion", "timeout": 1}, [PUBLIC_PAGE],
     {"Browser.getVersion": {"hang": True}}, {}),
    ("timeout_is_clamped_up", {"method": "Browser.getVersion", "timeout": 0.2}, [PUBLIC_PAGE],
     {"Browser.getVersion": {"hang": True}}, {}),
    ("method_required", {"params": {}}, [PUBLIC_PAGE], {}, {}),
    ("method_empty", {"method": ""}, [PUBLIC_PAGE], {}, {}),
    ("method_not_a_string", {"method": 5}, [PUBLIC_PAGE], {}, {}),
    ("params_list", {"method": "Browser.getVersion", "params": [1]}, [PUBLIC_PAGE], {}, {}),
    ("params_empty_list", {"method": "Browser.getVersion", "params": []}, [PUBLIC_PAGE],
     {"Browser.getVersion": {"result": {"product": "Chrome"}}}, {}),
    ("params_string", {"method": "Browser.getVersion", "params": "x"}, [PUBLIC_PAGE], {}, {}),
    ("params_number", {"method": "Browser.getVersion", "params": 3}, [PUBLIC_PAGE], {}, {}),
    ("frame_id_without_supervisor", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "frame_id": "F9"},
     [PUBLIC_PAGE], {}, {}),
    ("frame_id_private_page", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "frame_id": "T2"},
     [PRIVATE_PAGE], {}, {}),
    ("frame_id_private_navigate", {"method": "Page.navigate", "params": {"url": "http://10.0.0.1/"},
                                   "frame_id": "F9"}, [PUBLIC_PAGE], {}, {}),
    ("deep_result", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": nested(400)}}}}, {}),
    ("secret_in_key", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": {SECRET: 1}}}}}, {}),
    ("non_ascii_stays_unescaped", {"method": "Runtime.evaluate", "params": {"expression": "1"}, "target_id": "T1"},
     [PUBLIC_PAGE], {"Runtime.evaluate": {"result": {"result": {"value": "привет 世界"}}}}, {}),
]


class FakeBrowser:
    """A scripted CDP endpoint recording every frame it receives."""

    def __init__(self, targets, script):
        from websockets.sync.server import serve
        self.targets = targets
        self.script = script
        self.frames = []
        self.server = serve(self.handle, "127.0.0.1", 0, max_size=None)
        self.port = self.server.socket.getsockname()[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def url(self):
        return f"ws://127.0.0.1:{self.port}/devtools/browser/fixture"

    def handle(self, ws):
        try:
            for raw in ws:
                request = json.loads(raw)
                self.frames.append({key: request[key] for key in ("method", "params", "sessionId") if key in request})
                method = request["method"]
                reply = self.script.get(method)
                if method == "Target.getTargets" and reply is None:
                    reply = {"result": {"targetInfos": self.targets}}
                if method == "Target.attachToTarget" and reply is None:
                    reply = {"result": {"sessionId": "S1"}}
                if reply is None:
                    reply = {"result": {}}
                if reply.get("hang"):
                    continue
                if reply.get("noise"):
                    ws.send(json.dumps({"method": "Page.frameNavigated", "params": {"frame": {}}}))
                    ws.send(json.dumps({"id": request["id"] + 100, "result": {"stray": True}}))
                body = {"id": request["id"]}
                if "error" in reply:
                    body["error"] = reply["error"]
                elif "raw" in reply:
                    pass
                else:
                    body["result"] = reply.get("result", {})
                ws.send(json.dumps(body))
        except Exception:
            pass

    def close(self):
        self.server.shutdown()


def dump(fixture):
    """Indented JSON, with deeply nested values kept on one line each so the file stays small."""
    raws = {}

    def shrink(value, depth=0):
        if isinstance(value, (dict, list)) and depth > 8:
            token = f"@@RAW{len(raws)}@@"
            raws[token] = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
            return token
        if isinstance(value, dict):
            return {key: shrink(item, depth + 1) for key, item in value.items()}
        if isinstance(value, list):
            return [shrink(item, depth + 1) for item in value]
        return value

    text = json.dumps(shrink(fixture), indent=1, ensure_ascii=False)
    for token, raw in raws.items():
        text = text.replace(json.dumps(token), raw)
    return text + "\n"


def main():
    reference = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]).resolve()
    home = tempfile.mkdtemp()
    os.environ["HERMES_HOME"] = home
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    from tools import browser_cdp_tool as cdp
    from tools import browser_url_policy as policy

    fixture = {"url_literals": [], "metadata": [], "redact": [], "calls": []}
    for text in LITERAL_TEXTS:
        fixture["url_literals"].append({
            "text": text,
            "plain": policy.url_literals(text),
            "relative": policy.url_literals(text, protocol_relative=True)})
    for literal in METADATA_LITERALS:
        fixture["metadata"].append({"literal": literal, "blocked": bool(policy.literal_targets_metadata(literal))})
    for method, value in REDACT_CASES:
        fixture["redact"].append({"method": method, "value": value,
                                  "result": cdp._redact_cdp_method_result(method, value)})

    for name, arguments, targets, script, config in CALLS:
        fake = FakeBrowser(targets, script)
        os.environ["BROWSER_CDP_URL"] = fake.url
        (Path(home) / "config.yaml").write_text(
            "browser:\n  allow_private_urls: %s\n" % ("true" if config.get("allow_private_urls") else "false"))
        from tools import browser_session_cloud
        browser_session_cloud.reset_caches()
        raw = cdp.browser_cdp(
            method=arguments.get("method", ""), params=arguments.get("params"),
            target_id=arguments.get("target_id"), frame_id=arguments.get("frame_id"),
            timeout=arguments.get("timeout", 30.0), task_id=None)
        fake.close()
        fixture["calls"].append({"name": name, "arguments": arguments, "targets": targets, "script": script,
                                 "config": config, "result": json.loads(raw), "frames": fake.frames})
    out.write_text(dump(fixture), encoding="utf-8")
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
