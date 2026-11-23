#!/usr/bin/env python3
"""Record the MCP client behaviours of the Python reference: cancellation, session expiry,
SSE resumption and resources.

Usage: create_mcp_corpus.py <reference checkout> <mcp SDK site-packages> <out.json>
Run it with the reference interpreter, `-I`, from the reference checkout, with HERMES_HOME set to
an empty temporary directory. The reference interpreter does not carry the `mcp` package (the
reference pins mcp 2.3.0 in `uv.lock`), so the directory of an installed reference environment that
has it is added to the end of `sys.path`; it is only read. The loopback servers are
`compat/mcp_fixture_server.py` and `compat/mcp_http_fixture_server.py`, the same ones the Rust tests
use, so a Rust run can be compared with what the reference's own client did against them.

What is recorded:
  * `wire`: what the SDK's client sent for an abandoned call (the `notifications/cancelled` params,
    the headers), for a session that expired, for a stream that must be resumed (the GET and its
    headers, `retry` delays, the number of attempts), and the errors the call ended with.
  * `messages`: the text the model gets for each outcome, from the reference's own handlers
    (`_mcp_call_failed_message`, the session-expired outcome, the utility tools' errors).
  * `resources`: the schemas of the resource tools and their rendered output for sample answers.
  * `calls`: the text the model gets for `tools/call` answers (`_render_call_tool_result` with
    `for_model=True`, the registry handler's view), from wire dictionaries parsed by the SDK's own
    `CallToolResult`; the registry names (`mcp_prefixed_tool_name`); and the tool-call timeout
    (`_resolve_tool_timeout`) for each configuration.
"""

import asyncio
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HTTP_SERVER = ROOT / "compat" / "mcp_http_fixture_server.py"
STDIO_SERVER = ROOT / "compat" / "mcp_fixture_server.py"
TOKEN = "fixture-token"


def wait_for(predicate, what, seconds=10.0):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise RuntimeError(f"timed out waiting for {what}")


class HttpFixture:
    def __init__(self, *flags):
        import tempfile
        self.dir = tempfile.mkdtemp()
        self.log = Path(self.dir) / "log.json"
        self.process = subprocess.Popen(
            [sys.executable, "-I", str(HTTP_SERVER), "--token", TOKEN, "--log", str(self.log), *flags],
            stdout=subprocess.PIPE, text=True)
        self.url = self.process.stdout.readline().strip()

    def entries(self):
        try:
            return json.loads(self.log.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return []

    def stop(self):
        self.process.kill()
        self.process.wait()


async def http_session(fixture, httpx2, streamable_http_client, ClientSession):
    client = httpx2.AsyncClient(headers={"Authorization": f"Bearer {TOKEN}"}, follow_redirects=True,
                                timeout=httpx2.Timeout(10.0, read=300.0))
    cm = streamable_http_client(fixture.url, http_client=client, max_sse_event_size=8 * 1024 * 1024)
    return client, cm


def error_chain(exc):
    """`(type name, text)` of the exception the reference's failure message names."""
    from tools.mcp_tool_errors import _mcp_call_failed_message, _unwrap_exception_group
    root = _unwrap_exception_group(exc)
    return {"type": type(root).__name__, "text": str(root), "message": _mcp_call_failed_message(exc)}


async def record_http(sdk):
    httpx2, streamable_http_client, ClientSession = sdk
    out = {}

    async def with_session(flags, body):
        fixture = HttpFixture(*flags)
        try:
            client, cm = await http_session(fixture, httpx2, streamable_http_client, ClientSession)
            async with client:
                async with cm as streams:
                    async with ClientSession(streams[0], streams[1]) as session:
                        await session.initialize()
                        await session.list_tools()  # discovery warms the SDK's tool cache, as in the reference
                        return await body(fixture, session)
        finally:
            fixture.stop()

    # Abandoned call: the caller's task is cancelled, as the reference does on a timeout or an interrupt.
    async def cancelled(fixture, session):
        task = asyncio.ensure_future(session.call_tool("slow", arguments={}))
        await asyncio.to_thread(wait_for, lambda: any(
            e.get("params", {}).get("name") == "slow" for e in fixture.entries() if "params" in e), "call")
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        await asyncio.to_thread(wait_for, lambda: any(
            e.get("method") == "notifications/cancelled" for e in fixture.entries()), "cancel")
        return fixture.entries()
    entries = await with_session([], cancelled)
    call = next(e for e in entries if e.get("params", {}).get("name") == "slow")
    cancel = next(e for e in entries if e.get("method") == "notifications/cancelled")
    out["cancel_http"] = {
        "params_keys": sorted(cancel["params"]), "reason": cancel["params"]["reason"],
        "session_header": cancel["session"] == "fixture-session",
        "protocol": cancel["protocol"], "after_call": entries.index(cancel) > entries.index(call)}
    out["_cancel_http_request_id_is_a_number"] = isinstance(cancel["params"]["requestId"], int)

    # Session expired: the server forgets the session, the next request gets a 404.
    async def expired(fixture, session):
        await session.call_tool("expire", arguments={})
        try:
            await session.call_tool("echo", arguments={"text": "x"})
        except Exception as exc:
            return error_chain(exc), fixture.entries()
        return None, fixture.entries()
    error, entries = await with_session([], expired)
    out["session_expired"] = {"error": error}

    # A stream that ends before the response, with and without ids; every resumption outcome.
    async def streamed(tool):
        async def body(fixture, session):
            try:
                result = await session.call_tool(tool, arguments={})
                outcome = {"result": [b.text for b in result.content]}
            except Exception as exc:
                outcome = {"error": error_chain(exc)}
            entries = fixture.entries()
            gets = [e["get"] for e in entries if "get" in e and e["get"]["last_event_id"]]
            return {"outcome": outcome, "gets": gets}
        return await with_session([], body)
    out["resume"] = await streamed("resume")
    out["resume_twice"] = await streamed("resume_twice")
    out["resume_thrice"] = await streamed("resume_thrice")
    out["resume_primed"] = await streamed("resume_primed")
    out["resume_fail"] = await streamed("resume_fail")
    out["drop"] = await streamed("drop")
    return out


async def record_stdio(sdk_stdio, ClientSession):
    import tempfile
    stdio_client, StdioServerParameters = sdk_stdio
    log = Path(tempfile.mkdtemp()) / "log"
    params = StdioServerParameters(command=sys.executable, args=["-I", str(STDIO_SERVER)],
                                   env={"HERMES_MCP_LOG": str(log), "PATH": os.environ.get("PATH", "")})
    async with stdio_client(params) as streams:
        async with ClientSession(streams[0], streams[1]) as session:
            await session.initialize()
            await session.list_tools()
            task = asyncio.ensure_future(session.call_tool("slow", arguments={}))
            await asyncio.to_thread(wait_for, lambda: log.exists(), "call")
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
            await asyncio.to_thread(wait_for, lambda: "notifications/cancelled" in log.read_text(), "cancel")
    lines = [json.loads(line) for line in log.read_text().splitlines()]
    lines = [line for line in lines if "method" in line]
    cancel = next(line for line in lines if line["method"] == "notifications/cancelled")
    call = next(line for line in lines if line["method"] == "tools/call")
    return {"params_keys": sorted(cancel["params"]), "reason": cancel["params"]["reason"],
            "call_name": call["params"]["name"]}


async def record_resources(sdk):
    """Resource tool output for the answers of the fixture servers, through the SDK's own parsing."""
    httpx2, streamable_http_client, ClientSession = sdk
    from tools import mcp_tool_handlers as handlers
    from tools import mcp_tool_schema as schema
    from tools.mcp_tool import _paginate_full_list
    fixture = HttpFixture("--resources")
    out = {}
    try:
        client, cm = await http_session(fixture, httpx2, streamable_http_client, ClientSession)
        async with client:
            async with cm as streams:
                async with ClientSession(streams[0], streams[1]) as session:
                    init = await session.initialize()
                    capabilities = getattr(init, "capabilities", None)
                    out["advertises_resources"] = getattr(capabilities, "resources", None) is not None
                    listed = await _paginate_full_list(session.list_resources, "resources", "fx")
                    out["list_output"] = json.dumps(handlers._render_resource_list(listed, "fx"), ensure_ascii=False)
                    out["reads"] = {}
                    for uri in ("file:///notes/a.txt", "mem://plain", "file:///notes/b.bin"):
                        result = await session.read_resource(uri)
                        out["reads"][uri] = json.dumps(handlers._render_read_resource(result, "fx"), ensure_ascii=False)
                    try:
                        await session.read_resource("file:///missing")
                    except Exception as exc:
                        out["missing"] = error_chain(exc)
    finally:
        fixture.stop()
    out["schemas"] = [
        {"key": entry["handler_key"], "description": entry["schema"]["description"].replace("'fx'", "'{server}'"),
         "parameters": entry["schema"]["parameters"]}
        for entry in schema._build_utility_schemas("fx") if entry["handler_key"] in ("list_resources", "read_resource")]
    out["name_example"] = [entry["schema"]["name"] for entry in schema._build_utility_schemas("my-srv")][:2]
    # Rendering of answers the fixtures do not send: optional fields, hidden characters, binary blocks.
    from mcp import types
    hidden = "x\U000e0041y"
    resources = [
        types.Resource(uri="file:///a", name="a", description="d", mime_type="text/plain"),
        types.Resource(uri="file:///b", name="b"),
        types.Resource(uri="http://example.test", name=f"n{hidden}", description=""),
        types.Resource(uri="file:///über", name="ü"),
    ]
    out["render_list_cases"] = {
        "wire": [r.model_dump(by_alias=True, exclude_none=True, mode="json") for r in resources],
        "output": json.dumps(handlers._render_resource_list(resources, "fx"), ensure_ascii=False)}
    reads = types.ReadResourceResult(contents=[
        types.TextResourceContents(uri="file:///a", text=f"one{hidden}"),
        types.BlobResourceContents(uri="file:///b", blob="AAAA", mime_type="application/octet-stream"),
        types.TextResourceContents(uri="file:///c", text="")])
    out["render_read_cases"] = {
        "wire": reads.model_dump(by_alias=True, exclude_none=True, mode="json"),
        "output": json.dumps(handlers._render_read_resource(reads, "fx"), ensure_ascii=False)}
    out["empty_read"] = json.dumps(handlers._render_read_resource(types.ReadResourceResult(contents=[]), "fx"))
    out["empty_list"] = json.dumps(handlers._render_resource_list([], "fx"))
    return out


def record_calls():
    """`tools/call` rendering, registry names and the tool timeout, from the reference's own functions."""
    from agent import deadline
    from mcp import types
    from tools import mcp_tool_handlers as handlers
    from tools.mcp_tool_common import _resolve_tool_timeout
    from tools.mcp_tool_schema import mcp_prefixed_tool_name

    def text(value):
        return {"type": "text", "text": value}

    secret_text = "denied: Authorization Bearer abc123def and token=xyz789 in the request"
    cases = {
        "plain": {"content": [text("x")]},
        "empty": {"content": []},
        "multi": {"content": [text("one"), text(""), text("  "), text("two")]},
        "non_ascii": {"content": [text("привет \u2603 \U0001f600")]},
        "error": {"isError": True, "content": [text("tool failed")]},
        "error_empty": {"isError": True, "content": []},
        "error_blank_text": {"isError": True, "content": [text("")]},
        "error_multi": {"isError": True, "content": [text("a"), text("b")]},
        "error_secret": {"isError": True, "content": [text(secret_text)]},
        "error_hidden": {"isError": True, "content": [text("bad\U000e0041x")]},
        "error_resource": {"isError": True, "content": [
            {"type": "resource", "resource": {"uri": "file:///e", "text": "embedded failure"}}]},
        "error_image_only": {"isError": True, "content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]},
        "error_long": {"isError": True, "content": [text("e" * 3000)]},
        "structured_only": {"content": [], "structuredContent": {"a": 1, "b": [1.5, True, None], "c": "ü"}},
        "structured_dual": {"content": [text('{"a":1,"b":2}')], "structuredContent": {"a": 1, "b": 2}},
        "structured_dual_type_differs": {"content": [text('{"a":true}')], "structuredContent": {"a": 1}},
        "structured_dual_float": {"content": [text('{"a":1.0}')], "structuredContent": {"a": 1}},
        "structured_next_to_prose": {"content": [text("summary")], "structuredContent": {"a": 1, "z": 2}},
        "wrapped_string": {"content": [text("abc")], "structuredContent": {"result": "abc"}},
        "wrapped_number": {"content": [text("7")], "structuredContent": {"result": 7}},
        "wrapped_list": {"content": [text("1"), text("two")], "structuredContent": {"result": [1, "two"]}},
        "wrapped_list_mismatch": {"content": [text("1")], "structuredContent": {"result": [1, 2]}},
        "wrapped_other_key": {"content": [text("x")], "structuredContent": {"result": "x", "more": 1}},
        "meta_with_text": {"content": [text("x")], "_meta": {"com.example.mcp/k": 1, "plain": "v",
                           "modelcontextprotocol.io/r": 2, "tools.mcp.com/s": 3, "mcp/t": 4, "io.mcp/u": 5}},
        "meta_reserved_only": {"content": [text("x")], "_meta": {"modelcontextprotocol.io/r": 2}},
        "meta_only": {"content": [], "_meta": {"k": "v"}},
        "meta_and_structured": {"content": [], "structuredContent": {"s": 1}, "_meta": {"k": "v"}},
        "meta_structured_text": {"content": [text("t")], "structuredContent": {"s": 1}, "_meta": {"k": "v"}},
        "resource_link": {"content": [{"type": "resource_link", "uri": "file:///l", "name": "l", "mimeType": "text/plain"}]},
        "resource_text": {"content": [{"type": "resource", "resource": {"uri": "file:///r", "text": "body\U000e0041"}}]},
        "resource_blob": {"content": [{"type": "resource", "resource": {"uri": "file:///b", "blob": "AAAA", "mimeType": "x/y"}}]},
        "structured_floats": {"content": [], "structuredContent": {"a": 1e22, "b": 1e-7, "c": 1.25, "d": 100.0, "e": 1e16, "f": 1e15}},
        "hidden_text": {"content": [text("a\U000e0041b")]},
    }
    results = {}
    for name, wire in cases.items():
        result = types.CallToolResult.model_validate(wire)
        output = handlers._render_call_tool_result(result, "fx", [], for_model=True)
        # The wire is kept as text: the corpus file sorts keys, and the order of the server's keys is part of the output.
        # The document cache path of a saved binary block differs on every run.
        results[name] = {"wire": json.dumps(wire, ensure_ascii=False), "output": re.sub(r"saved to \S+ \(", "saved to <path> (", output)}
    names = [("srv", "echo"), ("my-server.v2", "do-it.now"), ("ünï", "tööl"), ("", ""), ("a b", "c/d"),
             ("srv", "list_resources"), ("x" * 40, "y" * 40), ("x" * 26, "y" * 26), ("x" * 27, "y" * 26),
             ("srv", "ü" * 70)]
    named = [{"server": s, "tool": t, "name": mcp_prefixed_tool_name(s, t)} for s, t in names]

    timeouts = []
    for config, section in [
            ({}, None), ({"timeout": 7}, None), ({"timeout": 2.5}, None), ({"timeout": 7}, {"mcp": {"tool_call": 9}}),
            ({}, {"mcp": {"tool_call": 9}}), ({}, {"mcp": {"tool_call": 45.5}}), ({}, {"mcp": {"tool_call": "30"}}),
            ({}, {"mcp": {"tool_call": 0}}), ({}, {"mcp": {"tool_call": -1}}), ({}, {"mcp": {"tool_call": True}}),
            ({}, {"mcp": {"tool_call": "soon"}}), ({}, {"mcp": {"tool_call": 1e12}}),
            ({}, {"mcp.tool_call": 9}), ({}, {"mcp": 9}), ({}, {"default": 9})]:
        deadline._timeouts_section = (lambda section=section: section or {})
        timeouts.append({"server": config, "timeouts": section, "resolved": _resolve_tool_timeout(config)})
    return {"results": results, "names": named, "timeouts": timeouts}


def record_messages():
    from tools import mcp_tool_handlers as handlers
    from tools.mcp_tool_errors import _is_session_expired_error
    messages = {
        "session_outcome_uncertain": handlers._SESSION_OUTCOME_UNCERTAIN_MSG,
        "tool_error_example": handlers.tool_error(
            handlers._SESSION_OUTCOME_UNCERTAIN_MSG.format(s="srv", state="been re-established"),
            outcome_uncertain=True, server="srv"),
        "missing_uri": handlers.tool_error("Missing required parameter 'uri'"),
        "not_connected": handlers.tool_error("MCP server 'srv' is not connected"),
        "interrupted": handlers.tool_error("MCP call interrupted: user sent a new message"),
        "expired_markers": {text: _is_session_expired_error(RuntimeError(text)) for text in (
            "Session terminated", "Not Found", "Server returned an error response",
            "SSE stream ended without a response", "Request 'tools/call' timed out")},
    }
    from tools.mcp_tool_loop import _run_on_mcp_loop  # noqa: F401 (the timeout text lives there)
    import inspect
    source = inspect.getsource(_run_on_mcp_loop)
    lines = source.splitlines()
    start = next(i for i, line in enumerate(lines) if "MCP call timed out" in line)
    messages["timeout_template"] = " ".join(line.strip() for line in lines[start:start + 2])
    return messages


def main():
    checkout, site, out_path = sys.argv[1:4]
    sys.path.insert(0, checkout)
    sys.path.append(site)
    sys.dont_write_bytecode = True
    import httpx2
    from mcp import ClientSession
    from mcp.client.stdio import StdioServerParameters, stdio_client
    from mcp.client.streamable_http import streamable_http_client
    sdk = (httpx2, streamable_http_client, ClientSession)
    version = __import__("importlib.metadata", fromlist=["version"]).version("mcp")

    async def run():
        wire = await record_http(sdk)
        wire["cancel_stdio"] = await record_stdio((stdio_client, StdioServerParameters), ClientSession)
        return wire, await record_resources(sdk)
    wire, resources = asyncio.run(run())
    # The document cache path of a saved binary resource differs on every run.
    text = re.sub(r"saved to \S+ \(", "saved to <path> (", json.dumps(resources))
    resources = json.loads(text)
    corpus = {"mcp_version": version, "wire": wire, "messages": record_messages(), "resources": resources,
              "calls": record_calls()}
    Path(out_path).write_text(json.dumps(corpus, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
                              encoding="utf-8")


if __name__ == "__main__":
    main()
