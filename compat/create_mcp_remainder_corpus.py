#!/usr/bin/env python3
"""Record MCP remainder behaviours from the Python reference:
- image/audio/document cache (`MEDIA:`, magic-byte sniffing, filenames, embedded blobs)
- hard result cap & structuredContent cap (`_render_call_tool_result` / `_capped_structured_content`)
- spillover layer (`maybe_persist_tool_result`, `_pageable_text`, `generate_preview`, `_safe_result_filename`)
- prompts utility schemas and rendering (`_render_prompt_list`, `_render_get_prompt`).

Usage: create_mcp_remainder_corpus.py <reference checkout> <mcp SDK site-packages> <output.json>
"""

import base64
import json
import os
import re
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace


def b64(raw: bytes) -> str:
    return base64.b64encode(raw).decode("ascii")


def main() -> int:
    if len(sys.argv) != 4:
        sys.stderr.write(
            "Usage: create_mcp_remainder_corpus.py <reference checkout> <mcp SDK site-packages> <output.json>\n"
        )
        return 2
    ref = Path(sys.argv[1]).resolve()
    site = Path(sys.argv[2]).resolve()
    out_path = Path(sys.argv[3]).resolve()
    home = Path(tempfile.mkdtemp(prefix="hermes_mcp_rem_"))
    os.environ["HERMES_HOME"] = str(home)
    sys.path.insert(0, str(ref))
    sys.path.append(str(site))
    sys.dont_write_bytecode = True

    from mcp import types
    from gateway.platforms.base_image_sniff import _looks_like_image
    from tools.audio_container import sniff_audio_ext
    from tools import mcp_tool_content as content_mod
    from tools import mcp_tool_handlers as handlers
    from tools import mcp_tool_schema as schema
    from tools import tool_result_storage as storage
    from tools.budget_config import BudgetConfig

    # 1. Image magic-byte sniffing
    png_bytes = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16
    jpeg_bytes = b"\xff\xd8\xff\xe0" + b"\x00" * 12
    gif87_bytes = b"GIF87a" + b"\x00" * 10
    gif89_bytes = b"GIF89a" + b"\x00" * 10
    bmp_bytes = b"BM" + b"\x00" * 14
    webp_bytes = b"RIFF\x10\x00\x00\x00WEBP" + b"\x00" * 4
    heic_bytes = b"\x00\x00\x00\x14ftypheic\x00\x00\x00\x00"
    avif_bytes = b"\x00\x00\x00\x14ftypavif\x00\x00\x00\x00"
    short_ftyp = b"\x00\x00\x00\x0cftypheic"
    html_bytes = b"<html><body>404</body></html>"
    plain_bytes = b"\x00\x00\x00"

    image_sniff_cases = []
    for label, raw in [
        ("png", png_bytes),
        ("jpeg", jpeg_bytes),
        ("gif87", gif87_bytes),
        ("gif89", gif89_bytes),
        ("bmp", bmp_bytes),
        ("webp", webp_bytes),
        ("heic", heic_bytes),
        ("avif", avif_bytes),
        ("short_ftyp", short_ftyp),
        ("html", html_bytes),
        ("short_3", plain_bytes),
        ("riff_not_webp", b"RIFF\x10\x00\x00\x00WAVE\x00\x00\x00\x00"),
    ]:
        image_sniff_cases.append({"label": label, "data_b64": b64(raw), "expected": _looks_like_image(raw)})

    # 2. Audio container sniffing
    audio_sniff_cases = []
    for label, raw, fallback in [
        ("m4a_brand", b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00", ".ogg"),
        ("mp4_generic_maps_m4a", b"\x00\x00\x00\x14ftypisom\x00\x00\x00\x00", ".ogg"),
        ("ogg", b"OggS\x00\x02", ".wav"),
        ("flac", b"fLaC\x00\x00", ".ogg"),
        ("mp3_id3", b"ID3\x03\x00", ".ogg"),
        ("mp3_sync", b"\xff\xfb\x90\x00", ".ogg"),
        ("aac_adts", b"\xff\xf1\x50\x80", ".ogg"),
        ("wav", b"RIFF\x24\x00\x00\x00WAVEfmt ", ".ogg"),
        ("webm", b"\x1a\x45\xdf\xa3\x9f\x42", ".ogg"),
        ("unknown_wav_fallback", b"not-audio-header", ".wav"),
        ("unknown_no_dot", b"not-audio-header", "mp3"),
    ]:
        audio_sniff_cases.append({
            "label": label,
            "data_b64": b64(raw),
            "fallback": fallback,
            "expected": sniff_audio_ext(raw, fallback),
        })

    # 3. Media & embedded resource rendering via _render_call_tool_result
    def normalize_paths(text: str) -> str:
        text = text.replace(str(home), "<HOME>")
        text = re.sub(r"(img|audio|doc)_[0-9a-f]{12}", r"\1_<HEX12>", text)
        return text

    media_cases = []
    for label, wire in [
        ("png_image", {"content": [{"type": "image", "data": b64(png_bytes), "mimeType": "image/png"}]}),
        ("jpeg_image", {"content": [{"type": "image", "data": b64(jpeg_bytes), "mimeType": "image/jpeg; charset=utf-8"}]}),
        ("non_image_rejected", {"content": [{"type": "image", "data": b64(html_bytes), "mimeType": "image/png"}]}),
        ("bad_b64_image", {"content": [{"type": "image", "data": "!!!not-base64!!!", "mimeType": "image/png"}]}),
        ("wav_audio", {"content": [{"type": "audio", "data": b64(b"RIFF\x24\x00\x00\x00WAVEfmt "), "mimeType": "audio/wav"}]}),
        ("ogg_audio_with_wav_mime", {"content": [{"type": "audio", "data": b64(b"OggS\x00\x02"), "mimeType": "audio/wav"}]}),
        ("unknown_audio_fallback", {"content": [{"type": "audio", "data": b64(b"rawbytes"), "mimeType": "audio/x-unknown"}]}),
        ("embedded_blob", {"content": [{"type": "resource", "resource": {
            "uri": "file:///reports/q1.pdf", "blob": b64(b"%PDF-1.4\n"), "mimeType": "application/pdf",
        }}]}),
        ("embedded_blob_bad_b64", {"content": [{"type": "resource", "resource": {
            "uri": "file:///bad.bin", "blob": "!!!", "mimeType": "application/octet-stream",
        }}]}),
        ("text_and_image", {"content": [
            {"type": "text", "text": "Screenshot taken"},
            {"type": "image", "data": b64(png_bytes), "mimeType": "image/png"},
        ]}),
    ]:
        res = types.CallToolResult.model_validate(wire)
        out = handlers._render_call_tool_result(res, "fx", [], for_model=True)
        media_cases.append({"label": label, "wire": wire, "expected": normalize_paths(out)})

    # 4. _mcp_resource_filename
    filename_cases = []
    for uri, mime in [
        ("file:///reports/q1.pdf", "application/pdf"),
        ("file:///a/b/../../etc/passwd", "text/plain"),
        ("https://example.test/dir/my%20file.txt?x=1", "text/plain"),
        ("file:///dir/bad\x00\x07\nname.bin", "application/octet-stream"),
        ("file:///", "application/pdf"),
        ("mem://only-host", "application/x-unknown-zzz"),
        ("file:///" + ("a" * 180) + ".json", "application/json"),
        ("file:///" + ("b" * 180), "application/octet-stream"),
    ]:
        filename_cases.append({
            "uri": uri,
            "mime": mime,
            "expected": content_mod._mcp_resource_filename(uri, mime),
        })

    # 5. Structured content cap & _render_call_tool_result with small synthetic cap
    orig_cap = content_mod._MCP_HARD_RESULT_CAP_CHARS
    orig_h_cap = handlers._MCP_HARD_RESULT_CAP_CHARS
    structured_cap_cases = []
    try:
        content_mod._MCP_HARD_RESULT_CAP_CHARS = 200
        handlers._MCP_HARD_RESULT_CAP_CHARS = 200
        # Monkeypatch default arg in _truncate_mcp_text_result by overriding call
        orig_trunc = content_mod._truncate_mcp_text_result
        def small_trunc(text: str, max_chars: int = 200) -> str:
            return orig_trunc(text, max_chars)
        handlers._truncate_mcp_text_result = small_trunc
        content_mod._truncate_mcp_text_result = small_trunc
        for label, wire in [
            ("structured_only_over_cap", {"content": [], "structuredContent": {"payload": "x" * 300}}),
            ("structured_and_text_over_cap", {"content": [{"type": "text", "text": "summary"}], "structuredContent": {"payload": "x" * 300}}),
            ("text_over_cap", {"content": [{"type": "text", "text": "a" * 150 + "b" * 150}]}),
        ]:
            res = types.CallToolResult.model_validate(wire)
            structured_cap_cases.append({
                "label": label,
                "cap": 200,
                "wire": wire,
                "expected": handlers._render_call_tool_result(res, "fx", [], for_model=True),
            })
    finally:
        content_mod._MCP_HARD_RESULT_CAP_CHARS = orig_cap
        handlers._MCP_HARD_RESULT_CAP_CHARS = orig_h_cap
        handlers._truncate_mcp_text_result = orig_trunc
        content_mod._truncate_mcp_text_result = orig_trunc

    # 6. Spillover layer: _pageable_text, generate_preview, _safe_result_filename, maybe_persist_tool_result
    pageable_cases = []
    for label, raw in [
        ("mcp_result_only", json.dumps({"result": "line1\nline2\nline3"})),
        ("mcp_result_with_structured_and_meta", json.dumps({
            "result": "hello\nworld", "structuredContent": {"k": 1}, "_meta": {"v": "ok"},
        })),
        ("mcp_empty_result", json.dumps({"result": "", "structuredContent": {"k": 1}})),
        ("plain_text", "just plain text"),
    ]:
        pageable_cases.append({"label": label, "input": raw, "expected": storage._pageable_text(raw)})

    preview_cases = []
    for label, text, max_chars in [
        ("fits", "short text", 50),
        ("nl_second_half", "aaaa\nbbbb\ncccc\ndddd", 12),
        ("nl_first_half_only", "aa\n" + "b" * 30, 20),
        ("no_nl", "x" * 30, 10),
    ]:
        p, has_more = storage.generate_preview(text, max_chars=max_chars)
        preview_cases.append({"label": label, "text": text, "max_chars": max_chars, "preview": p, "has_more": has_more})

    safe_id_cases = []
    for raw_id in ["call_123", "bad/id:with spaces!", "x" * 150, "", "..."]:
        safe_id_cases.append({"raw_id": raw_id, "expected": storage._safe_result_filename(raw_id)})

    big_mcp_json = json.dumps({"result": "".join(f"row {i:04d} value\n" for i in range(400)), "_meta": {"src": "fx"}})
    cfg = BudgetConfig(mcp_result_size=1000, preview_size=200)
    persisted_msg = storage.maybe_persist_tool_result(big_mcp_json, "mcp__fx__big", "call-spill-1", config=cfg)
    spilled_path = storage.extract_persisted_path(persisted_msg)
    spilled_body = Path(spilled_path).read_text(encoding="utf-8") if spilled_path else None
    inline_fallback = storage._inline_truncated("prev", "mcp__fx__big", "x" * 12345)

    # 7. Prompts schemas & rendering
    prompt_schemas = [
        {"key": e["handler_key"], "description": e["schema"]["description"].replace("'fx'", "'{server}'"),
         "parameters": e["schema"]["parameters"]}
        for e in schema._build_utility_schemas("fx")
        if e["handler_key"] in ("list_prompts", "get_prompt")
    ]
    hidden = "x\U000e0041y"
    prompts_objs = [
        types.Prompt(
            name="summarize",
            description=f"Summarize {hidden}",
            arguments=[
                types.PromptArgument(name="topic", description=f"Topic {hidden}", required=True),
                types.PromptArgument(name="tone", required=False),
            ],
        ),
        types.Prompt(name="bare"),
    ]
    prompts_wire = [p.model_dump(by_alias=True, exclude_none=True, mode="json") for p in prompts_objs]
    prompts_list_out = json.dumps(handlers._render_prompt_list(prompts_objs, "fx"), ensure_ascii=False)

    prompt_result = types.GetPromptResult(
        description=f"Prompt desc {hidden}",
        messages=[
            types.PromptMessage(role="user", content=types.TextContent(type="text", text=f"Hello {hidden}")),
            types.PromptMessage(role="assistant", content=types.TextContent(type="text", text="Ready")),
        ],
    )
    prompt_get_wire = prompt_result.model_dump(by_alias=True, exclude_none=True, mode="json")
    prompt_get_out = json.dumps(handlers._render_get_prompt(prompt_result, "fx"), ensure_ascii=False)

    payload = {
        "image_sniff_cases": image_sniff_cases,
        "audio_sniff_cases": audio_sniff_cases,
        "media_cases": media_cases,
        "filename_cases": filename_cases,
        "structured_cap_cases": structured_cap_cases,
        "pageable_cases": pageable_cases,
        "preview_cases": preview_cases,
        "safe_id_cases": safe_id_cases,
        "spillover_example": {
            "input": big_mcp_json,
            "threshold": 1000,
            "preview_size": 200,
            "tool_call_id": "call-spill-1",
            "message": persisted_msg.replace(str(home), "<HOME>"),
            "spilled_body": spilled_body,
            "inline_fallback": inline_fallback,
        },
        "prompts": {
            "schemas": prompt_schemas,
            "list_wire": prompts_wire,
            "list_output": prompts_list_out,
            "get_wire": prompt_get_wire,
            "get_output": prompt_get_out,
        },
    }
    out_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Wrote MCP remainder corpus to {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
