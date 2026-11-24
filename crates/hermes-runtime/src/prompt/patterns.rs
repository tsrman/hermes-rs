// Patterns from NousResearch/hermes-agent tools/threat_patterns.py.
// Reference commit: 2f8c1e7ed051c311af4f3b3ca0f8976b9c40839b.

// MIT License
//
// Copyright (c) 2025 Nous Research
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

pub(super) const NEGATED_FORGET: &str = r#"(?:^|[^\w*’'])[*_]*(?:(?:do|does|did|ca|could|would|should|wo|must|need|is|are|was|were|ai|has|have|had)n[’']?t|cannot|not|never)[*_]*[ \t]*(?:\r?\n[ \t]*)?(?:ever[ \t]+)?[*_]*(?P<verb>forget)(?:$|[\W_])"#;

// The third field marks rules used only for persistent memory. Whitespace runs
// have already been folded. Word spans use a single loop to keep Unicode
// classes compact, with the overall scan bounded by the input file limit.
#[rustfmt::skip]
pub(super) const RULES: &[(&str, &str, bool)] = &[
    ("prompt_injection", r#"ignore\s(?:\w+\s){0,8}(previous|all|above|prior)\s(?:\w+\s){0,8}instructions"#, false),
    ("sys_prompt_override", r#"system\sprompt\soverride"#, false),
    ("disregard_rules", r#"disregard\s(?:\w+\s){0,8}(your|all|any)\s(?:\w+\s){0,8}(instructions|rules|guidelines)"#, false),
    ("forget_rules", r#"(?:^|[^\w’']|_)forget\s(?:about\s)?(your|all|any|previous|prior|above|earlier)\s(?:\w+\s){0,3}(instructions|rules|guidelines)(?:$|[\W_])"#, false),
    ("bypass_restrictions", r#"act\sas\s(if|though)\s(?:\w+\s){0,8}you\s(?:\w+\s){0,8}(have\sno|don\'t\shave)\s(?:\w+\s){0,8}(restrictions|limits|rules)"#, false),
    ("html_comment_injection", r#"<!--[^>]{0,512}(?:ignore|override|system|secret|hidden)[^>]{0,512}-->"#, false),
    ("hidden_div", r#"<\s?div\sstyle\s?=\s?["\'][^>]{0,2048}display\s?:\s?none"#, false),
    ("translate_execute", r#"translate\s[^\n]{0,512}\sinto\s\w+(?:[\s-]{1,64}\w+){0,2}\sand\s(execute|run|eval)\b"#, false),
    ("deception_hide", r#"do\snot\s(?:\w+\s){0,8}tell\s(?:\w+\s){0,8}the\suser"#, false),
    ("role_hijack", r#"you\sare\s(?:\w+\s){0,8}now\s(?:a|an|the)\s"#, false),
    ("role_pretend", r#"pretend\s(?:\w+\s){0,8}(you\sare|to\sbe)\s"#, false),
    ("leak_system_prompt", r#"output\s(?:\w+\s){0,8}(system|initial)\sprompt"#, false),
    ("remove_filters", r#"(respond|answer|reply)\swithout\s(?:\w+\s){0,8}(restrictions|limitations|filters|safety)"#, false),
    ("fake_update", r#"you\shave\sbeen\s(?:\w+\s){0,8}(updated|upgraded|patched)\sto"#, false),
    ("identity_override", r#"\bname\syourself\s\w+"#, false),
    ("c2_node_registration", r#"register\s(as\s)?a?\s?node"#, false),
    ("c2_heartbeat", r#"(heartbeat|beacon|check[\s\-]?in)\s(to|with)\s"#, false),
    ("c2_task_pull", r#"pull\s(down\s)?(?:new\s)?task(?:ing|s)?\b"#, false),
    ("c2_network_connect", r#"connect\sto\sthe\snetwork\b"#, false),
    ("forced_action", r#"you\smust\s(?:\w+\s){0,3}(register|connect|report|beacon)\b"#, false),
    ("anti_forensic_oneliner", r#"only\suse\sone[\s\-]?liners?\b"#, false),
    ("anti_forensic_disk", r#"never\s(?:\w+\s){0,8}(?:create|write)\s(?:\w+\s){0,8}(?:script|file)\s(?:\w+\s){0,8}disk"#, false),
    ("env_var_unset_agent", r#"unset\s\w*(?:CLAUDE|CODEX|HERMES|AGENT|OPENAI|ANTHROPIC)\w*"#, false),
    ("known_c2_framework", r#"\b(?:cobalt\s?strike|sliver|havoc|mythic|metasploit|brainworm)\b"#, false),
    ("c2_explicit", r#"\bc2\s(?:server|channel|infrastructure|beacon)\b"#, false),
    ("c2_explicit_long", r#"\bcommand\sand\scontrol\b"#, false),
    ("exfil_curl", r#"curl\s[^\n]{0,2048}\$\{?\w*(?:KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL)S?\b"#, false),
    ("exfil_wget", r#"wget\s[^\n]{0,2048}\$\{?\w*(?:KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL)S?\b"#, false),
    ("read_secrets", r#"cat\s[^\n]{0,2048}(\.env|credentials|\.netrc|\.pgpass|\.npmrc|\.pypirc)"#, false),
    ("send_to_url", r#"(send|post|upload|transmit)\s[^\n]{0,2048}\s(to|at)\shttps?://"#, true),
    ("context_exfil", r#"(include|output|print|share)\s(?:\w+\s){0,8}(conversation|chat\shistory|previous\smessages|full\scontext|entire\scontext)"#, true),
    ("ssh_backdoor", r#"authorized_keys"#, true),
    ("ssh_access", r#"(?:\b(?:echo|cat|cp|mv|dd|tee|install|printf|rsync|scp|ln|append|add|write|sed|chmod|chown|truncate|rm|touch|curl|wget|git)\b|\bopen\s?\(|>>?)[^\n]{0,512}(?:\$HOME/\.ssh|~/\.ssh)"#, true),
    ("hermes_env", r#"\$HOME/\.(?:hermes|hsr)/\.env|\~/\.(?:hermes|hsr)/\.env"#, true),
    ("agent_config_mod", r#"(update|modify|edit|write|change|append|add\sto)\s[^\n]{0,2048}(?:AGENTS\.md|CLAUDE\.md|\.cursorrules|\.clinerules)"#, true),
    ("hermes_config_mod", r#"(update|modify|edit|write|change|append|add\sto)\s[^\n]{0,2048}\.(?:hermes|hsr)/(config\.yaml|SOUL\.md)"#, true),
    ("hardcoded_secret", r#"(?:api[_-]?key|token|secret|password)\s?[=:]\s?["\'](?P<value>[A-Za-z0-9+/=_-]{20,})"#, true),
];
