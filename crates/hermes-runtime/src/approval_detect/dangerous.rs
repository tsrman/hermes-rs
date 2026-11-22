//! The dangerous-command layer (`detect_dangerous_command`, `detect_content_rule_keys` and
//! `_iter_dangerous_descriptions` of `approval_detection.py`). Pattern rows come from
//! `dangerous_tables`, generated from the reference.
//!
//! Not ported: the macOS `osascript` classifier (`approval_applescript.py`) and the
//! `cron.lifecycle_guard` splice check (`_is_shell_token_spliced_gateway_lifecycle`); a command that
//! only those would flag is judged by the other rules here. The reference's lazy generator is
//! driven to the first match for the verdict; variants are collected up front.

use std::sync::LazyLock;

use fancy_regex::Regex;

use super::budget::{take_recursion_limit, with_budget, BudgetExceeded, Text};
use super::command_starts::{
    iter_shell_command_word_spans, match_redirection, shell_command_segment,
};
use super::dangerous_tables::{
    CONTENT_RULE_DESCRIPTIONS, DANGEROUS_PATTERNS, HIDDEN_VARIATION_SELECTOR, PATTERN_KEY_ALIASES,
    PS_QUOTED_SCRIPT_LAUNCHERS, QUOTE_MASKED_DANGEROUS,
};
use super::exec_findings::execution_flag_findings;
use super::hardline::{
    command_detection_variants, detect_hardline_command, mask_quoted_prose,
    PARSER_LIMIT_DESCRIPTION,
};
use super::matchers::{
    char_at, dev_socket_redirect, download_then_run, pipe_remote_to_shell, wmic_shadow_delete,
};
use super::normalize::{lower_preserving_flags, normalize_command_for_detection, HomeEnv};
use super::py_isspace;
use super::shell_words::{
    deobfuscate_shell_word_for_detection, read_shell_word, shlex_split, skip_shell_whitespace,
};
use super::text_masks::command_parser_limit_exceeded;

pub const SCAN_INCOMPLETE_DESCRIPTION: &str =
    "command too complex to scan fully; review before running";
pub const CREDENTIAL_UPLOAD_DESCRIPTION: &str =
    "upload a secret or credential file via curl/wget (possible exfiltration)";
pub const INVISIBLE_UNICODE_DESCRIPTION: &str =
    "invisible or bidirectional Unicode control character (possible obfuscation)";
const PS_PIPE_ALIAS_DESCRIPTION: &str = "PowerShell destructive delete (alias after pipe)";
const PIPE_REMOTE_TO_SHELL: &str = "pipe remote content to shell";
const WMIC_SHADOW_DELETE: &str = "delete volume shadow copies (wmic)";
const DEV_TCP_REDIRECT: &str = "reverse shell via /dev/tcp or /dev/udp redirect";
const DOWNLOAD_THEN_RUN: &str = "execute a just-downloaded file";

type R<T> = Result<T, BudgetExceeded>;

enum Kind2 {
    Regex(Regex),
    PipeRemote,
    Wmic,
    DevSocket,
    DownloadRun,
}

struct Rule {
    kind: Kind2,
    description: &'static str,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    DANGEROUS_PATTERNS
        .iter()
        .map(|&(pattern, description)| {
            let kind = match description {
                PIPE_REMOTE_TO_SHELL => Kind2::PipeRemote,
                WMIC_SHADOW_DELETE => Kind2::Wmic,
                DEV_TCP_REDIRECT => Kind2::DevSocket,
                DOWNLOAD_THEN_RUN => Kind2::DownloadRun,
                _ => Kind2::Regex(
                    Regex::new(&format!("(?is){pattern}"))
                        .unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}")),
                ),
            };
            Rule { kind, description }
        })
        .collect()
});

impl Rule {
    fn search(&self, text: &str) -> R<bool> {
        match &self.kind {
            Kind2::Regex(re) => re.is_match(text).map_err(|_| BudgetExceeded),
            Kind2::PipeRemote => pipe_remote_to_shell(text),
            Kind2::Wmic => wmic_shadow_delete(text, 0),
            Kind2::DevSocket => dev_socket_redirect(text),
            Kind2::DownloadRun => download_then_run(text, false),
        }
    }
}

/// `fnmatch.fnmatchcase(text, pattern)`.
pub fn fnmatchcase(text: &str, pattern: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let mut re = String::from(r"\A");
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        i += 1;
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                let mut j = i;
                if j < p.len() && p[j] == '!' {
                    j += 1;
                }
                if j < p.len() && p[j] == ']' {
                    j += 1;
                }
                while j < p.len() && p[j] != ']' {
                    j += 1;
                }
                if j >= p.len() {
                    re.push_str(r"\[");
                } else {
                    let mut stuff: String = p[i..j].iter().collect::<String>().replace('\\', r"\\");
                    i = j + 1;
                    if let Some(rest) = stuff.strip_prefix('!') {
                        stuff = format!("^{rest}");
                    } else if stuff.starts_with('^') {
                        stuff = format!(r"\{stuff}");
                    }
                    re.push_str(&format!("[{stuff}]"));
                }
            }
            other => re.push_str(&fancy_regex::escape(&other.to_string())),
        }
    }
    re.push_str(r"\z");
    Regex::new(&format!("(?s){re}")).is_ok_and(|r| r.is_match(text).unwrap_or(false))
}

fn is_env_assignment(word: &str) -> bool {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\A[A-Za-z_][A-Za-z0-9_]*=[^\n]*\z").expect("valid pattern")
    });
    RE.is_match(word)
}

/// True for a curl/wget word, or one whose command name holds an unresolved expansion.
fn word_may_run_curl_or_wget(word: &str) -> R<bool> {
    let deob = deobfuscate_shell_word_for_detection(word)?;
    let names = [
        deob.rsplit('/').next().unwrap_or("").to_owned(),
        word.rsplit(['\\', '/']).next().unwrap_or("").to_owned(),
    ];
    for name in &names {
        let lowered = name.to_lowercase();
        let stripped = lowered.strip_suffix(".exe").unwrap_or(&lowered);
        if stripped == "curl" || stripped == "wget" {
            return Ok(true);
        }
    }
    for name in &names {
        if name.contains(['*', '?', '[']) {
            let lowered = name.to_lowercase();
            if ["curl", "wget", "curl.exe", "wget.exe"]
                .iter()
                .any(|tool| fnmatchcase(tool, &lowered))
            {
                return Ok(true);
            }
        }
    }
    let unquoted = word.trim_start_matches(['\'', '"']);
    if unquoted.starts_with(['$', '`']) {
        return Ok(true);
    }
    Ok(!unquoted.starts_with('-')
        && !is_env_assignment(word)
        && (word.contains('$') || word.contains('`')))
}

/// Merged `(starts, ends)` of the ranges where an upload flag may belong to curl/wget.
fn upload_flag_owner_ranges(command: &str) -> R<(Vec<usize>, Vec<usize>)> {
    let text = Text::new(command);
    let chars = &text.chars;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (start, _, _) in iter_shell_command_word_spans(&text)? {
        let end = start + shell_command_segment(&text, start)?.chars().count();
        let mut pos = start;
        while pos < end {
            if let Some(redirect_end) = match_redirection(chars, skip_shell_whitespace(chars, pos))
            {
                pos = read_shell_word(&text, redirect_end)?.1;
                continue;
            }
            let (word_start, word_end, word) = read_shell_word(&text, pos)?;
            pos = word_end;
            if word_start == pos {
                break;
            }
            if word_may_run_curl_or_wget(&word)? {
                ranges.push((word_start, end));
                break;
            }
            if chars[word_start..pos].iter().any(|c| py_isspace(*c)) {
                ranges.push((word_start + 1, pos));
            }
        }
    }
    ranges.sort_unstable();
    let (mut starts, mut ends): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    for (range_start, range_end) in ranges {
        match ends.last_mut() {
            Some(last) if range_start <= *last => *last = (*last).max(range_end),
            _ => {
                starts.push(range_start);
                ends.push(range_end);
            }
        }
    }
    Ok((starts, ends))
}

fn upload_flag_owned(ranges: &(Vec<usize>, Vec<usize>), flag_start: usize) -> bool {
    let index = ranges.0.partition_point(|&s| s <= flag_start);
    index > 0 && flag_start < ranges.1[index - 1]
}

/// Whether a launcher (ssh, cmd /c, iex, a shell carrier, pwsh) at command position runs the
/// quoted delete as code.
fn quoted_text_is_executed(variant: &str, rule: &Rule) -> R<bool> {
    static CMD_C: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)\Acmd(?:\.exe)?\s+/[ck]\b").expect("valid pattern"));
    let text = Text::new(variant);
    let mut prev_start: Option<usize> = None;
    for (start, _, word) in iter_shell_command_word_spans(&text)? {
        let stage_start = prev_start;
        prev_start = Some(start);
        let deob = deobfuscate_shell_word_for_detection(&word)?;
        let lowered = deob.rsplit('/').next().unwrap_or("").to_lowercase();
        let name = lowered.strip_suffix(".exe").unwrap_or(&lowered);
        if !PS_QUOTED_SCRIPT_LAUNCHERS.contains(&name) {
            continue;
        }
        let segment = lower_preserving_flags(&shell_command_segment(&text, start)?);
        if name == "cmd" && !CMD_C.is_match(&segment).unwrap_or(false) {
            continue;
        }
        if rule.search(&segment)? {
            return Ok(true);
        }
        let before: String = text.chars[stage_start.unwrap_or(0)..start]
            .iter()
            .collect::<String>()
            .trim_end_matches(py_isspace)
            .to_owned();
        if (name == "pwsh" || name == "powershell")
            && stage_start.is_some()
            && before.ends_with('|')
            && !before.ends_with("||")
            && rule.search(&lower_preserving_flags(&before))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_verification_artifact_cleanup(command: &str) -> bool {
    let Some(argv) = shlex_split(command) else {
        return false;
    };
    if argv.len() != 3 || argv[0] != "rm" || argv[1] != "-f" {
        return false;
    }
    let operand = &argv[2];
    let resolve =
        |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let temp_dir = resolve(&std::env::temp_dir());
    let operand_path = std::path::Path::new(operand);
    let basename = operand_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\Ahermes-(?:verify|ad-hoc)-[A-Za-z0-9_.-]+\z").expect("valid pattern")
    });
    let parent = operand_path.parent().map(resolve);
    operand_path == temp_dir.join(&basename)
        && parent.as_deref() == Some(temp_dir.as_path())
        && NAME.is_match(&basename)
}

/// Every dangerous match in detection order (`stop_at_first` ends the scan at the first one).
/// `only` restricts the scan to those content rules.
fn scan(
    command: &str,
    only: Option<&[&str]>,
    stop_at_first: bool,
    env: &HomeEnv,
) -> R<Vec<String>> {
    let mut found: Vec<String> = Vec::new();
    macro_rules! emit {
        ($d:expr) => {{
            found.push($d.to_string());
            if stop_at_first {
                return Ok(found);
            }
        }};
    }
    if command_parser_limit_exceeded(command) {
        if only.is_none() {
            emit!(PARSER_LIMIT_DESCRIPTION);
        }
        return Ok(found);
    }
    if is_verification_artifact_cleanup(command) {
        return Ok(found);
    }
    let rules: Vec<&Rule> = RULES
        .iter()
        .filter(|r| only.is_none_or(|o| o.contains(&r.description)))
        .collect();
    let mut capped = Vec::new();
    for variant in command_detection_variants(command, 0, &mut capped, env, false)? {
        let command_lower = lower_preserving_flags(&variant);
        let mut masked_lower: Option<String> = None;
        for rule in &rules {
            let description = rule.description;
            if QUOTE_MASKED_DANGEROUS.contains(&description)
                || description == PS_PIPE_ALIAS_DESCRIPTION
            {
                if masked_lower.is_none() {
                    masked_lower = Some(lower_preserving_flags(&mask_quoted_prose(&variant)?));
                }
                let masked = masked_lower.as_deref().unwrap_or("");
                if rule.search(masked)?
                    || (description == PS_PIPE_ALIAS_DESCRIPTION
                        && rule.search(&command_lower)?
                        && quoted_text_is_executed(&variant, rule)?)
                {
                    emit!(description);
                }
            } else if description == CREDENTIAL_UPLOAD_DESCRIPTION {
                let Kind2::Regex(re) = &rule.kind else {
                    continue;
                };
                let matches: Vec<usize> = re
                    .find_iter(&command_lower)
                    .filter_map(Result::ok)
                    .map(|m| char_at(&command_lower, m.start()))
                    .collect();
                let ranges = if matches.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    upload_flag_owner_ranges(&command_lower)?
                };
                if matches.iter().any(|&m| upload_flag_owned(&ranges, m)) {
                    emit!(description);
                }
            } else if rule.search(&command_lower)? {
                emit!(description);
            }
        }
    }
    if !capped.is_empty() && only.is_none() {
        emit!(PARSER_LIMIT_DESCRIPTION);
    }
    static SELECTOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(HIDDEN_VARIATION_SELECTOR).expect("valid pattern"));
    if let Some(only) = only {
        if only.contains(&INVISIBLE_UNICODE_DESCRIPTION)
            && SELECTOR.is_match(command).map_err(|_| BudgetExceeded)?
        {
            emit!(INVISIBLE_UNICODE_DESCRIPTION);
        }
        return Ok(found);
    }
    let normalized = normalize_command_for_detection(command, env);
    for (description, _) in execution_flag_findings(&normalized)? {
        emit!(description);
    }
    if SELECTOR.is_match(command).map_err(|_| BudgetExceeded)? {
        emit!(INVISIBLE_UNICODE_DESCRIPTION);
    }
    Ok(found)
}

/// Verdict of the dangerous layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerousVerdict {
    pub dangerous: bool,
    /// The first match: the pattern key, which is also its description.
    pub description: Option<String>,
}

/// Check the dangerous patterns; the first match decides.
pub fn detect_dangerous_command(command: &str, env: &HomeEnv) -> DangerousVerdict {
    let len = command.chars().count();
    take_recursion_limit();
    let mut description = match with_budget(len, || scan(command, None, true, env)) {
        Ok(found) => found.into_iter().next(),
        Err(_) if take_recursion_limit() => Some(PARSER_LIMIT_DESCRIPTION.to_owned()),
        Err(_) => Some(SCAN_INCOMPLETE_DESCRIPTION.to_owned()),
    };
    if description.is_none() && !detect_hardline_command(command, env).complete {
        description = Some(SCAN_INCOMPLETE_DESCRIPTION.to_owned());
    }
    DangerousVerdict {
        dangerous: description.is_some(),
        description,
    }
}

/// Every dangerous pattern key the command matches, in detection order and without repeats. The
/// reference asks for consent on the first match plus the content rules only; asking for each match
/// stops an approved verb from waving a second one through.
pub fn detect_all_dangerous_keys(command: &str, env: &HomeEnv) -> Vec<String> {
    let len = command.chars().count();
    take_recursion_limit();
    match with_budget(len, || scan(command, None, false, env)) {
        Ok(found) => {
            let mut out: Vec<String> = Vec::new();
            for d in found {
                if !out.contains(&d) {
                    out.push(d);
                }
            }
            out
        }
        Err(_) if take_recursion_limit() => vec![PARSER_LIMIT_DESCRIPTION.to_owned()],
        Err(_) => vec![SCAN_INCOMPLETE_DESCRIPTION.to_owned()],
    }
}

/// Every key the command needs consent for: all dangerous matches, plus the scan-incomplete key when
/// the hardline scan could not read the command to the end (a grant never covers that one).
pub fn dangerous_keys(command: &str, env: &HomeEnv, hardline_complete: bool) -> Vec<String> {
    let mut keys = detect_all_dangerous_keys(command, env);
    if !hardline_complete && !keys.iter().any(|k| k == SCAN_INCOMPLETE_DESCRIPTION) {
        keys.push(SCAN_INCOMPLETE_DESCRIPTION.to_owned());
    }
    keys
}

/// Pattern keys of every content-level rule the command matches, in detection order.
pub fn detect_content_rule_keys(command: &str, env: &HomeEnv) -> Vec<String> {
    let len = command.chars().count();
    take_recursion_limit();
    match with_budget(len, || {
        scan(command, Some(CONTENT_RULE_DESCRIPTIONS), false, env)
    }) {
        Ok(found) => {
            let mut out: Vec<String> = Vec::new();
            for d in found {
                if !out.contains(&d) {
                    out.push(d);
                }
            }
            out
        }
        Err(_) => {
            take_recursion_limit();
            Vec::new()
        }
    }
}

/// All approval keys for a pattern: its description plus the historical regex-derived key that
/// older allowlist and session entries may still use (`_approval_key_aliases`).
pub fn approval_key_aliases(pattern_key: &str) -> Vec<String> {
    PATTERN_KEY_ALIASES
        .iter()
        .find(|(key, _)| *key == pattern_key)
        .map_or_else(
            || vec![pattern_key.to_owned()],
            |(_, aliases)| aliases.iter().map(|a| (*a).to_owned()).collect(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_keys_resolve_both_ways() {
        let aliases = approval_key_aliases("git branch force delete");
        assert!(aliases.contains(&"git branch force delete".to_owned()));
        assert!(aliases.contains(&r"git\s+branch\s+-D".to_owned()));
        let back = approval_key_aliases(r"git\s+branch\s+-D");
        assert!(back.contains(&"git branch force delete".to_owned()));
        assert_eq!(
            approval_key_aliases("unknown key"),
            vec!["unknown key".to_owned()]
        );
    }
}
