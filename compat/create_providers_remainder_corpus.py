#!/usr/bin/env python3
"""Record exact reference behaviors for Task 17 (providers remainder: NVIDIA, OAuth, Bedrock, Vertex)."""

import json
import os
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

REF_ROOT = Path("/home/tsr/ref/hermes-main")
sys.path.insert(0, str(REF_ROOT))

OUT_PATH = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "hermes-runtime"
    / "tests"
    / "fixtures"
    / "providers_remainder_cases.json"
)


def main():
    cases = {}
    with tempfile.TemporaryDirectory() as tmp:
        os.environ["HERMES_HOME"] = tmp

        # 1. NVIDIA prepare_messages
        from providers import get_provider_profile

        nvidia = get_provider_profile("nvidia")
        nvidia_cases = []
        for label, msgs in [
            ("no_tool_msgs", [{"role": "user", "content": "hi", "name": "alice"}]),
            ("tool_with_name", [
                {"role": "assistant", "content": None, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "name": "read_file", "content": "ok"},
            ]),
            ("tool_with_both", [
                {"role": "tool", "tool_call_id": "c2", "name": "a", "tool_name": "b", "content": "res"},
                {"role": "user", "name": "keep_me", "content": "next"},
            ]),
            ("tool_clean", [
                {"role": "tool", "tool_call_id": "c3", "content": "clean"},
            ]),
        ]:
            nvidia_cases.append({
                "label": label,
                "input": msgs,
                "output": nvidia.prepare_messages(msgs),
            })
        cases["nvidia"] = {
            "name": nvidia.name,
            "aliases": list(nvidia.aliases),
            "base_url": nvidia.base_url,
            "env_vars": list(nvidia.env_vars),
            "prepare_messages": nvidia_cases,
        }

        # 2. Vertex AI
        from agent.vertex_adapter import build_vertex_base_url

        cases["vertex"] = {
            "base_urls": [
                {"project_id": p, "region": r, "url": build_vertex_base_url(p, r)}
                for p, r in [
                    ("my-proj", "global"),
                    ("my-proj", "us-central1"),
                    ("acme-123", "europe-west4"),
                ]
            ]
        }

        # 3. AWS Bedrock
        from agent import bedrock_adapter as ba

        bedrock_models = []
        for mid in [
            "openai.gpt-5.4",
            "us.openai.gpt-5.4",
            "in.openai.gpt-5.6-terra",
            "openai.gpt-oss-120b-1:0",
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "us.anthropic.claude-opus-4-6-v1:0",
            "global.anthropic.claude-haiku-4-5-v1:0",
            "amazon.nova-2-lite-v1:0",
        ]:
            parsed = ba.parse_bedrock_openai_model_id(mid)
            bedrock_models.append({
                "model_id": mid,
                "parsed_openai": list(parsed) if parsed else None,
                "uses_mantle": ba.bedrock_openai_uses_mantle(mid),
                "is_anthropic": ba.is_anthropic_bedrock_model(mid),
            })

        converse_tools = ba.convert_tools_to_converse([
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
                },
            }
        ])

        converse_msg_cases = []
        for label, msgs in [
            ("basic_system_and_user", [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"},
            ]),
            ("tool_roundtrip_and_merging", [
                {"role": "system", "content": "Sys1"},
                {"role": "system", "content": "   "},
                {"role": "user", "content": "Run two tools"},
                {"role": "assistant", "content": "Calling...", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": '{"path":"a.txt"}'}},
                    {"id": "call_2", "type": "function", "function": {"name": "read_file", "arguments": '{"path":"b.txt"}'}},
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "content A"},
                {"role": "tool", "tool_call_id": "call_2", "content": ""},
            ]),
            ("assistant_leading_and_trailing", [
                {"role": "assistant", "content": "Prefill"},
            ]),
        ]:
            sys_blocks, conv_msgs = ba.convert_messages_to_converse(msgs, model="amazon.nova-2-lite-v1:0")
            converse_msg_cases.append({
                "label": label,
                "input": msgs,
                "system": sys_blocks,
                "messages": conv_msgs,
            })

        norm_cases = []
        for label, raw in [
            ("text_and_cache_usage", {
                "output": {"message": {"role": "assistant", "content": [{"text": "Hello"}, {"text": "world"}]}},
                "stopReason": "end_turn",
                "usage": {"inputTokens": 100, "outputTokens": 25, "cacheReadInputTokens": 40, "cacheWriteInputTokens": 10},
            }),
            ("tool_use", {
                "output": {"message": {"role": "assistant", "content": [
                    {"text": "Checking."},
                    {"toolUse": {"toolUseId": "tu_1", "name": "read_file", "input": {"path": "x.rs"}}},
                ]}},
                "stopReason": "tool_use",
                "usage": {"inputTokens": 50, "outputTokens": 15},
            }),
        ]:
            norm = ba.normalize_converse_response(raw)
            msg = norm.choices[0].message
            norm_cases.append({
                "label": label,
                "input": raw,
                "content": msg.content,
                "finish_reason": norm.choices[0].finish_reason,
                "tool_calls": [
                    {"id": tc.id, "name": tc.function.name, "arguments": tc.function.arguments}
                    for tc in (msg.tool_calls or [])
                ],
                "prompt_tokens": norm.usage.prompt_tokens,
                "completion_tokens": norm.usage.completion_tokens,
            })

        cases["bedrock"] = {
            "models": bedrock_models,
            "mantle_url_us_east_1": ba.bedrock_openai_base_url("us-east-1"),
            "converse_tools": converse_tools,
            "converse_messages": converse_msg_cases,
            "normalize_response": norm_cases,
        }

        # 4. OAuth helpers
        from hermes_cli.auth_constants import _decode_jwt_claims
        from agent.anthropic_credentials import _is_oauth_token, anthropic_route_is_oauth
        from agent.anthropic_adapter import _oauth_wire_namer, _CLAUDE_CODE_SYSTEM_PREFIX, _OAUTH_ONLY_BETAS
        from hermes_cli.auth_nous import _validate_nous_inference_url_from_network
        from hermes_cli.auth_minimax import _minimax_resolve_token_expiry_unix

        jwt_cases = []
        # eyJhbGciOiJub25lIn0.eyJleHAiOjE5MDAwMDAwMDAsInN1YiI6InUxIn0.sig
        for tok in [
            "eyJhbGciOiJub25lIn0.eyJleHAiOjE5MDAwMDAwMDAsInN1YiI6InUxIn0.sig",
            "not-a-jwt",
            "a.!!!.c",
            "",
        ]:
            jwt_cases.append({"token": tok, "claims": _decode_jwt_claims(tok)})

        anth_oauth_cases = []
        for key, url, prov in [
            ("sk-ant-api03-abc", "https://api.anthropic.com", "anthropic"),
            ("sk-ant-usr01-abc", "https://api.anthropic.com", "anthropic"),
            ("sk-ant-svc01-abc", "https://api.anthropic.com", "anthropic"),
            ("sk-ant-admin01-abc", "https://api.anthropic.com", "anthropic"),
            ("sk-ant-oat01-secret", "https://api.anthropic.com", "anthropic"),
            ("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxIn0.s", "https://api.anthropic.com", "anthropic"),
            ("cc-token-123", "https://api.anthropic.com", "anthropic"),
            ("sk-ant-oat01-secret", "https://proxy.example.com/anthropic", "custom"),
        ]:
            anth_oauth_cases.append({
                "key": key,
                "base_url": url,
                "provider": prov,
                "is_oauth_token": _is_oauth_token(key),
                "route_is_oauth": anthropic_route_is_oauth(url, key, provider=prov),
            })

        to_wire = _oauth_wire_namer([
            {"name": "read_file"},
            {"name": "session_search"},
            {"name": "memory"},
            {"name": "mcp__srv__tool"},
            {"name": "mcp_legacy"},
        ])
        wire_names = {
            name: to_wire(name)
            for name in ["read_file", "session_search", "memory", "mcp__srv__tool", "mcp_legacy"]
        }

        nous_urls = [
            {"url": u, "validated": _validate_nous_inference_url_from_network(u)}
            for u in [
                "https://inference-api.nousresearch.com/v1/",
                "https://welcome-api.nousresearch.com/v1",
                "http://inference-api.nousresearch.com/v1",
                "https://evil.example.com/v1",
            ]
        ]

        now_dt = datetime(2026, 11, 23, 12, 0, 0, tzinfo=timezone.utc)
        minimax_exp = [
            {"expired_in": 3600, "unix": _minimax_resolve_token_expiry_unix(3600, now=now_dt)},
            {"expired_in": 1800000000000, "unix": _minimax_resolve_token_expiry_unix(1800000000000, now=now_dt)},
        ]

        cases["oauth"] = {
            "jwt_claims": jwt_cases,
            "anthropic_oauth": anth_oauth_cases,
            "anthropic_system_prefix": _CLAUDE_CODE_SYSTEM_PREFIX,
            "anthropic_oauth_betas": _OAUTH_ONLY_BETAS,
            "anthropic_wire_names": wire_names,
            "nous_urls": nous_urls,
            "minimax_expiry_now": now_dt.timestamp(),
            "minimax_expiry": minimax_exp,
        }

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(cases, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Wrote {OUT_PATH}")


if __name__ == "__main__":
    main()
