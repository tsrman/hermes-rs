#!/usr/bin/env python3
"""Record Task 15 reference outputs from the Python Hermes reference repo.

Records:
- tools/website_policy.py (_normalize_rule, _extract_host_from_urlish, _match_host_against_rule,
  load_website_blocklist, check_website_access)
- plugins/web/keyless_mcp.py (_parse_mcp_body, _parse_exa_search_text, _is_rate_limitish,
  _is_search_failover_eligible, search_with_failover, extract_with_failover, keyless vendor shapes)
- plugins/web/openai_native/provider.py and plugins/web/ddgs/provider.py error/shape contracts
"""

import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import MagicMock, patch

REF = Path("/home/tsr/ref/hermes-main")
VENV_SITE = Path("/home/tsr/ref/hermes-reference-venv/lib/python3.14/site-packages")
OUT = Path(__file__).resolve().parent.parent / "crates/hermes-runtime/tests/fixtures/web_remainder_cases.json"

sys.path.insert(0, str(REF))
if VENV_SITE.exists():
    sys.path.append(str(VENV_SITE))

from tools import website_policy
from plugins.web import keyless_mcp
from plugins.web.openai_native.provider import OpenAINativeWebSearchProvider
from plugins.web.ddgs.provider import DDGSWebSearchProvider


def record_website_policy():
    norm_inputs = [
        "Example.COM",
        "www.Example.COM",
        "https://www.Example.COM/path/to/page?x=1",
        "http://Sub.Site.ORG./foo",
        "*.tracker.net",
        "example.com/some/path",
        "   ",
        "# comment.example.com",
        "www.",
        "https://www.sub.domain.test:8443/a",
    ]
    normalize_cases = [
        {"input": s, "expected": website_policy._normalize_rule(s)}
        for s in norm_inputs
    ]

    host_inputs = [
        "https://Example.COM:8443/path",
        "http://sub.site.org./foo",
        "example.com/a/b",
        "//cdn.example.com/x",
        "https://[::1]:8080/a",
        "https://www.Example.org/",
        "",
        "not a url",
    ]
    extract_host_cases = [
        {"url": u, "expected": website_policy._extract_host_from_urlish(u)}
        for u in host_inputs
    ]

    match_pairs = [
        ("example.com", "example.com"),
        ("sub.example.com", "example.com"),
        ("deep.sub.example.com", "example.com"),
        ("notexample.com", "example.com"),
        ("example.com.evil.org", "example.com"),
        ("a.tracker.net", "*.tracker.net"),
        ("a.b.tracker.net", "*.tracker.net"),
        ("tracker.net", "*.tracker.net"),
        ("other.net", "*.tracker.net"),
        ("", "example.com"),
        ("example.com", ""),
    ]
    match_cases = [
        {"host": h, "pattern": p, "expected": website_policy._match_host_against_rule(h, p)}
        for h, p in match_pairs
    ]

    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        shared = home / "shared_blocklist.txt"
        shared.write_text("\ufeff# Shared list\n\nwww.Shared-Bad.org/path\n*.ads.test\nexample.com\n", encoding="utf-8")
        cfg = home / "config.yaml"
        cfg.write_text(
            "security:\n"
            "  website_blocklist:\n"
            "    enabled: true\n"
            "    domains:\n"
            "      - Example.COM\n"
            "      - https://www.Blocked.io/secret\n"
            "      - '# ignored'\n"
            "    shared_files:\n"
            "      - shared_blocklist.txt\n"
            "      - missing_file_ignored.txt\n",
            encoding="utf-8",
        )
        old_home = os.environ.get("HERMES_HOME")
        os.environ["HERMES_HOME"] = str(home)
        try:
            check_urls = [
                "https://example.com/page",
                "https://sub.example.com/page",
                "https://blocked.io/a",
                "https://www.blocked.io/a",
                "https://shared-bad.org/x",
                "https://banner.ads.test/img.png",
                "https://ads.test/",
                "https://allowed.org/docs",
                "https://notexample.com/",
            ]
            check_cases = []
            for u in check_urls:
                res = website_policy.check_website_access(u, config_path=cfg)
                if res is not None:
                    res = {
                        k: v.replace(str(home), "<HOME>") if isinstance(v, str) else v
                        for k, v in res.items()
                    }
                check_cases.append({"url": u, "expected": res})

            # Disabled policy allows everything
            cfg_off = home / "config_off.yaml"
            cfg_off.write_text(
                "security:\n  website_blocklist:\n    enabled: false\n    domains: [example.com]\n",
                encoding="utf-8",
            )
            disabled_result = website_policy.check_website_access("https://example.com/", config_path=cfg_off)

            # Missing enabled key defaults to False
            cfg_default = home / "config_default.yaml"
            cfg_default.write_text(
                "security:\n  website_blocklist:\n    domains: [example.com]\n",
                encoding="utf-8",
            )
            default_enabled_result = website_policy.check_website_access("https://example.com/", config_path=cfg_default)
        finally:
            if old_home is None:
                os.environ.pop("HERMES_HOME", None)
            else:
                os.environ["HERMES_HOME"] = old_home

    return {
        "normalize_cases": normalize_cases,
        "extract_host_cases": extract_host_cases,
        "match_cases": match_cases,
        "check_cases": check_cases,
        "disabled_result": disabled_result,
        "default_enabled_result": default_enabled_result,
    }


def record_keyless_mcp():
    mcp_body_inputs = [
        ("plain_json", json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "hello world"}]}})),
        ("sse_json", 'event: message\ndata: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"sse ok"}]}}\n\n'),
        ("sse_u2028_preserved", 'event: message\r\ndata: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"a\u2028b"}]}}\r\n\r\n'),
        ("jsonrpc_error", json.dumps({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "rate limit hit"}})),
        ("is_error_true", json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"isError": True, "content": [{"type": "text", "text": "Too many requests"}]}})),
        ("is_error_empty", json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"isError": True, "content": []}})),
        ("empty_content", json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"content": []}})),
        ("unrecognized", "<html>502 Bad Gateway</html>"),
    ]
    mcp_body_cases = []
    for label, body in mcp_body_inputs:
        try:
            mcp_body_cases.append({"label": label, "body": body, "ok": True, "value": keyless_mcp._parse_mcp_body(body)})
        except keyless_mcp.KeylessMCPError as exc:
            mcp_body_cases.append({"label": label, "body": body, "ok": False, "error": str(exc)})

    exa_text = (
        "Title: Rust Programming Language\n"
        "URL: https://www.rust-lang.org/\n"
        "Published: 2026-01-01\n"
        "Highlights:\n"
        "A language empowering everyone\n"
        "to build reliable and efficient software.\n"
        "Author: Rust Team\n"
        "\n---\n"
        "Title: Missing URL Block\n"
        "Highlights:\n"
        "Should be skipped\n"
        "\n---\n"
        "Title: Cargo Book\n"
        "URL: https://doc.rust-lang.org/cargo/\n"
        "Highlights:\n"
        "The Rust package manager\n"
        "\n---\n"
        "Title: Third Hit\n"
        "URL: https://example.org/3\n"
        "Highlights:\n"
        "Trimmed by limit\n"
    )
    exa_parse_cases = [
        {"limit": 2, "text": exa_text, "expected": keyless_mcp._parse_exa_search_text(exa_text, 2)},
        {"limit": 5, "text": exa_text, "expected": keyless_mcp._parse_exa_search_text(exa_text, 5)},
    ]

    failover_inputs = [
        "Keyless Exa search failed: HTTP 429: Too Many Requests",
        "Rate limit exceeded for anonymous user",
        "Keyless Parallel search failed: HTTP 403: Forbidden",
        "Keyless Keenable search failed: HTTP 503: Service Unavailable",
        "Keyless Exa search failed: HTTP 401: Unauthorized",
        "Keyless Exa search failed: HTTP 402: Payment Required",
        "Keyless Exa search failed: HTTP 400: invalid query 'http 503'",
        "Keyless Exa search failed: HTTP 404: Not Found",
        "invalid query syntax",
        "slow down please",
        "quota exceeded",
    ]
    failover_cases = [
        {
            "message": msg,
            "rate_limitish": keyless_mcp._is_rate_limitish(msg),
            "search_failover_eligible": keyless_mcp._is_search_failover_eligible(msg),
        }
        for msg in failover_inputs
    ]

    # Record failover & vendor message shapes with mocked helpers
    with patch.object(keyless_mcp, "provider_tier", return_value="auto"), \
         patch.object(keyless_mcp, "_vendor_pinned", return_value=True):
        with patch.dict(keyless_mcp._KEYLESS_SEARCHERS, {
            "exa": lambda q, l: {"success": False, "error": "Keyless Exa search failed: HTTP 429: limit. Set EXA_API_KEY (https://exa.ai) or another web backend via `hermes tools` for reliable service."},
            "parallel": lambda q, l: {"success": True, "data": {"web": [{"url": "https://p.test", "title": "P", "description": "d", "position": 1}]}},
            "firecrawl": lambda q, l: {"success": False, "error": "Keyless Firecrawl search failed: HTTP 503"},
            "keenable": lambda q, l: {"success": False, "error": "Keyless Keenable search failed: HTTP 503"},
        }):
            failover_success = keyless_mcp.search_with_failover("exa", "rust", 5)

        with patch.dict(keyless_mcp._KEYLESS_SEARCHERS, {
            "exa": lambda q, l: {"success": False, "error": "Keyless Exa search failed: HTTP 429"},
            "parallel": lambda q, l: {"success": False, "error": "Keyless Parallel search failed: HTTP 503"},
            "firecrawl": lambda q, l: {"success": False, "error": "Keyless Firecrawl search failed: HTTP 403"},
            "keenable": lambda q, l: {"success": False, "error": "Keyless Keenable search failed: HTTP 429"},
        }):
            failover_exhausted = keyless_mcp.search_with_failover("exa", "rust", 5)

        with patch.dict(keyless_mcp._KEYLESS_EXTRACTORS, {
            "exa": lambda urls: [{"url": u, "title": "", "content": "", "error": "HTTP 429 from origin"} for u in urls],
            "parallel": lambda urls: [{"url": u, "title": "ShouldNotRun", "content": "x"} for u in urls],
        }):
            extract_origin_429_no_failover = keyless_mcp.extract_with_failover("exa", ["https://example.com"])

    with patch.object(keyless_mcp, "provider_tier", return_value="paid"), \
         patch.object(keyless_mcp, "_vendor_pinned", return_value=True):
        all_paid_search = keyless_mcp.search_with_failover("exa", "rust", 5)
        all_paid_extract = keyless_mcp.extract_with_failover("exa", ["https://example.com"])

    return {
        "mcp_body_cases": mcp_body_cases,
        "exa_parse_cases": exa_parse_cases,
        "failover_cases": failover_cases,
        "failover_success": failover_success,
        "failover_exhausted": failover_exhausted,
        "extract_origin_429_no_failover": extract_origin_429_no_failover,
        "all_paid_search": all_paid_search,
        "all_paid_extract": all_paid_extract,
        "fail_msg_exa_search": keyless_mcp._fail_msg("exa", "search", "HTTP 429"),
        "fail_msg_firecrawl_extract": keyless_mcp._fail_msg("firecrawl", "extract", "HTTP 429", other_backends=False),
    }


def record_openai_native_and_ddgs():
    native = OpenAINativeWebSearchProvider()
    ddgs = DDGSWebSearchProvider()
    return {
        "openai_native_search_error": native.search("rust", 5),
        "openai_native_display_name": native.display_name,
        "ddgs_display_name": ddgs.display_name,
    }


def main():
    corpus = {
        "website_policy": record_website_policy(),
        "keyless_mcp": record_keyless_mcp(),
        "providers": record_openai_native_and_ddgs(),
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(corpus, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
