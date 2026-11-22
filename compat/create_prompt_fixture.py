#!/usr/bin/env python3
"""Record the static parts of the reference system prompt and the results of its gates.

Usage: create_prompt_fixture.py <reference checkout> <text.json> <cases.json>
Run it with the reference interpreter. The JSON holds the paragraphs (identity, help pointer, task and
parallel-call guidance, tool-use enforcement, execution guidance with its per-line tool conditions,
the CLI platform hint, the WSL hint, the coding brief with its edit-format lines) and, for a grid of
model names and tool sets, what the reference's own gate functions return, so the Rust port can be
checked against them.
"""
import json
import os
import sys
import tempfile
from pathlib import Path

reference = Path(sys.argv[1]).resolve()
text_out = Path(sys.argv[2]).resolve()
cases_out = Path(sys.argv[3]).resolve()
os.environ["HERMES_HOME"] = tempfile.mkdtemp()
sys.path.insert(0, str(reference))
os.chdir(reference)

from agent import coding_context as cc
from agent import prompt_builder as pb
from agent import system_prompt as sp

MODELS = [
    "fixture", "deepseek-flash-1017", "gpt-5", "GPT-4.1", "codex-mini", "claude-sonnet-5-5", "gemini-2.5-pro",
    "gemma-3", "grok-4", "glm-4.7", "qwen3-coder", "kimi-k2", "minimax-m2", "mistral-large", "muse-spark",
    "llama-4", "hermes-4", "o3", "", "mimo-v2",
]
SETTINGS = [True, False, "auto", "AUTO", "true", "off", "Yes", "never", ["gpt", "foo"], ["FIXTURE"], [], None, 5]
TOOL_SETS = [
    [], ["terminal"], ["read_file"], ["search_files"], ["execute_code"], ["terminal", "read_file"],
    ["web_search"], ["write_file", "patch"],
]

gates = []
for setting in SETTINGS:
    for model in MODELS:
        gates.append({
            "setting": setting, "model": model,
            "enforcement": sp._model_gate(setting, model, pb.TOOL_USE_ENFORCEMENT_MODELS),
            "execution": sp._model_gate(setting, model, pb.EXECUTION_GUIDANCE_MODELS),
        })

execution_texts = [
    {"tools": tools, "text": pb.execution_guidance_text(set(tools))} for tools in TOOL_SETS
] + [{"tools": None, "text": pb.execution_guidance_text(None)}]

briefs = []
for model in MODELS:
    for todo in (True, False):
        names = {"todo_list"} if todo else set()
        mode = cc.RuntimeMode(profile=cc.CODING_PROFILE, surface="cli", cwd=Path("/nonexistent"),
                              config_mode="auto", model=model, instructions="")
        prefix, _, _ = mode.system_prompt_parts(valid_tool_names=names, workspace_block="")
        briefs.append({"model": model, "todo_list": todo, "brief": prefix[0]})

data = {
    "identity": pb.DEFAULT_AGENT_IDENTITY,
    "help_guidance": pb.HERMES_AGENT_HELP_GUIDANCE,
    "help_guidance_no_skills": pb.HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS,
    "task_completion": pb.TASK_COMPLETION_GUIDANCE,
    "parallel_tool_calls": pb.PARALLEL_TOOL_CALL_GUIDANCE,
    "session_search_guidance": pb.SESSION_SEARCH_GUIDANCE,
    "skills_guidance": pb.SKILLS_GUIDANCE,
    "tool_use_enforcement": pb.TOOL_USE_ENFORCEMENT_GUIDANCE,
    "google_operational": pb.GOOGLE_MODEL_OPERATIONAL_GUIDANCE,
    "execution_guidance": pb.OPENAI_MODEL_EXECUTION_GUIDANCE,
    "execution_line_tools": {line: sorted(tools) for line, tools in pb._EXECUTION_GUIDANCE_LINE_TOOLS.items()},
    "enforcement_models": list(pb.TOOL_USE_ENFORCEMENT_MODELS),
    "execution_models": list(pb.EXECUTION_GUIDANCE_MODELS),
    "platform_hint_cli": pb.PLATFORM_HINTS["cli"],
    "wsl_hint": pb.WSL_ENVIRONMENT_HINT,
    "coding_guidance": cc.CODING_AGENT_GUIDANCE,
    "todo_sentence": cc._TODO_SENTENCE,
    "no_todo_sentence": cc._NO_TODO_SENTENCE,
    "edit_format": {name: {"needles": list(needles), "line": line}
                    for name, (needles, line) in cc._EDIT_FORMAT_GUIDANCE.items()},
    "workspace_header": cc.WORKSPACE_BLOCK_HEADER,
    "runtime_heading": pb.RUNTIME_ENVIRONMENT_HEADING,
    "runtime_end": pb.RUNTIME_ENVIRONMENT_END,
    "project_markers": list(cc._PROJECT_MARKERS),
    "context_files": list(cc._CONTEXT_FILES),
    "code_extensions": sorted(cc._CODE_EXTENSIONS),
    "scan_skip_dirs": sorted(cc._CODE_SCAN_SKIP_DIRS),
    "py_lockfiles": [list(pair) for pair in cc._PY_LOCKFILES],
    "js_lockfiles": [list(pair) for pair in cc._JS_LOCKFILES],
    "verify_targets": list(cc._VERIFY_TARGETS),
}
text_out.write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
cases = {"gates": gates, "execution_texts": execution_texts, "briefs": briefs}
cases_out.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
print("wrote", text_out, cases_out)
