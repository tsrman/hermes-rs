//! The hardline floor: commands that never run through the agent, whatever `--yolo` or the
//! approval mode says (`_detect_hardline_command`, the detection-variant generators of
//! `approval_detection.py`, `hardline_detection_variants` of `approval_wrappers.py`).
//!
//! The reference's variant generators are lazy; here they are collected in the same order, so
//! the first match is the same and only the work spent after a budget trip can differ.

use std::collections::HashSet;
use std::sync::LazyLock;

use fancy_regex::Regex;

use super::ansi_c::expand_ansi_c_quotes;
use super::budget::{take_recursion_limit, with_budget, BudgetExceeded, Text};
use super::command_starts::{
    iter_shell_command_word_spans, mark_command_starts, shell_command_segment, splice,
};
use super::env_split::env_split_payload;
use super::exec_findings::{execution_flag_findings, nested_payloads, MALFORMED_EXEC_DESCRIPTION};
use super::hardline_tables::{
    HARDLINE_PATTERNS, WHATIF_OTHER_HARDLINE, WHATIF_STATEMENT_UNGUARDED, WHATIF_STRICT_PATTERNS,
};
use super::matchers::download_then_run;
use super::normalize::{normalize_command_for_detection, HomeEnv};
use super::shell_heredoc::mask_heredoc_bodies_for_shell_scan;
use super::shell_scan::{scan_shell, Kind, ScanOpts};
use super::shell_tokens::{
    grep_safe_detection_variant, iter_top_level_shell_segments, shell_segment_tokens,
};
use super::shell_words::deobfuscate_shell_word_for_detection;
use super::text_masks::{
    command_parser_limit_exceeded, join_escaped_word_chars, mask_quoted_newlines,
};
use super::wrapper_prefix::{
    blank_literal_heredoc_bodies, dequote_rm_operands, literal_heredoc_bodies,
    strip_wrapper_prefixes,
};

pub const PARSER_LIMIT_DESCRIPTION: &str = "command parser limit exceeded";
pub const FALLBACK_MATCH_SUFFIX: &str =
    " (a quick re-read of a command too complex to scan fully; quoted text can match)";
const MAX_CARRIER_DEPTH: usize = 3;
const SHELL_CARRIER_NAMES: [&str; 7] = ["eval", "sh", "bash", "zsh", "ksh", "dash", "source"];

type R<T> = Result<T, BudgetExceeded>;
type Span = (usize, usize, String);

struct Compiled {
    regex: Regex,
    description: &'static str,
    quote_masked: bool,
}

fn compile(pattern: &str, flags: &str) -> Regex {
    Regex::new(&format!("{flags}{pattern}"))
        .unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}"))
}

static PATTERNS: LazyLock<Vec<Compiled>> = LazyLock::new(|| {
    HARDLINE_PATTERNS
        .iter()
        .map(|&(pattern, description, quote_masked)| Compiled {
            regex: compile(pattern, "(?is)"),
            description,
            quote_masked,
        })
        .collect()
});

/// The same table with the `-WhatIf` exemption removed from the two PowerShell rules.
static PATTERNS_NO_WHATIF: LazyLock<Vec<Compiled>> = LazyLock::new(|| {
    HARDLINE_PATTERNS
        .iter()
        .map(|&(pattern, description, quote_masked)| {
            let strict = WHATIF_STRICT_PATTERNS
                .iter()
                .find(|(d, _)| *d == description)
                .map_or(pattern, |(_, p)| *p);
            Compiled {
                regex: compile(strict, "(?is)"),
                description,
                quote_masked,
            }
        })
        .collect()
});

fn search(regex: &Regex, text: &str) -> R<bool> {
    regex.is_match(text).map_err(|_| BudgetExceeded)
}

/// Whether every guarded PowerShell cmdlet in `command` previews with its own real `-WhatIf` and no
/// other destructive verb appears anywhere.
pub fn whatif_preview_is_real(command: &str) -> R<bool> {
    static OTHER: LazyLock<Regex> = LazyLock::new(|| compile(WHATIF_OTHER_HARDLINE, "(?i)"));
    static UNGUARDED: LazyLock<Regex> =
        LazyLock::new(|| compile(WHATIF_STATEMENT_UNGUARDED, "(?i)"));
    if search(&OTHER, command)? {
        return Ok(false);
    }
    for statement in command.split(['\r', '\n', '\u{85}', '\u{2028}', '\u{2029}', ';', '&', '|']) {
        if search(&UNGUARDED, statement)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Blank out quoted string content for positionless hardline matching. Quote characters stay;
/// inside double quotes `$(...)` and backtick spans stay raw because the shell runs them.
pub fn mask_quoted_prose(command: &str) -> R<String> {
    let text = Text::new(command);
    let opts = ScanOpts {
        subst_quoted: true,
        naive_backtick: true,
        ..ScanOpts::default()
    };
    let mut out = String::new();
    for step in scan_shell(&text, 0, None, opts) {
        let step = step?;
        let j = step.j.unwrap_or(step.i + 1);
        if step.quote.is_none() || matches!(step.kind, Kind::Quote | Kind::Subst) {
            out.extend(&text.chars[step.i..j]);
        } else {
            out.extend(std::iter::repeat_n(' ', j - step.i));
        }
    }
    Ok(out)
}

/// Whether any command-position word is a shell-carrying command (`eval`, `sh -c`, `source`, `.`).
pub fn contains_shell_carrier(command: &str) -> R<bool> {
    let text = Text::new(command);
    for (_, _, word) in iter_shell_command_word_spans(&text)? {
        let name = deobfuscate_shell_word_for_detection(&word)?;
        let base = name.rsplit('/').next().unwrap_or("").to_lowercase();
        if SHELL_CARRIER_NAMES.contains(&base.as_str()) || base == "." {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The `-c` / `--command` string of a `su` / `runuser` argv.
fn su_command_payload(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        index += 1;
        if token == "--" {
            return None;
        }
        let (option, equals, value) = match token.split_once('=') {
            Some((o, v)) => (o, true, v),
            None => (token.as_str(), false, ""),
        };
        if option == "--command" || option == "--session-command" {
            return if equals {
                Some(value.to_owned())
            } else {
                args.get(index).cloned()
            };
        }
        if [
            "--shell",
            "--group",
            "--supp-group",
            "--whitelist-environment",
            "--user",
        ]
        .contains(&option)
        {
            index += usize::from(!equals);
            continue;
        }
        let mut chars = token.chars();
        if !(chars.next() == Some('-') && chars.next().is_some_and(|c| c.is_ascii_alphabetic())) {
            continue;
        }
        let rest: Vec<char> = token.chars().skip(1).collect();
        for (offset, c) in rest.iter().enumerate() {
            if *c == 'c' {
                let attached: String = rest[offset + 1..].iter().collect();
                return if attached.is_empty() {
                    args.get(index).cloned()
                } else {
                    Some(attached)
                };
            }
            if "sgGwu".contains(*c) {
                if offset + 1 >= rest.len() {
                    index += 1;
                }
                break;
            }
        }
    }
    None
}

/// Literal command strings that `env -S`, `su -c`, `runuser -c` or a shell `-c` would run.
fn carried_payloads(command: &str) -> R<Vec<String>> {
    let mut out = Vec::new();
    for (_, payload) in execution_flag_findings(command)? {
        if let Some(payload) = payload.filter(|p| !p.is_empty()) {
            out.push(payload);
        }
    }
    for segment in iter_top_level_shell_segments(command)? {
        let seg_text = Text::new(&segment);
        for (start, _, word) in iter_shell_command_word_spans(&seg_text)? {
            let deob = deobfuscate_shell_word_for_detection(&word)?;
            let name = deob.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
            if !["env", "su", "runuser"].contains(&name.as_str()) {
                continue;
            }
            let bounded = shell_command_segment(&seg_text, start)?;
            let Some(tokens) = shell_segment_tokens(&bounded, 0).filter(|t| !t.is_empty()) else {
                continue;
            };
            let payload = if name == "env" {
                env_split_payload(&tokens, 0)
            } else {
                su_command_payload(&tokens[1..])
            };
            if let Some(payload) = payload.filter(|p| !p.is_empty()) {
                out.push(payload);
            }
        }
    }
    Ok(out)
}

/// The data-dropped, code-dropped and (for floors, or when a dropped `cat` body's file runs later)
/// raw copies of `command`.
pub fn heredoc_scan_sources(command: &str, floors: bool) -> R<Vec<String>> {
    let data_dropped = mask_heredoc_bodies_for_shell_scan(command, false);
    let code_dropped = mask_heredoc_bodies_for_shell_scan(command, true);
    let include_raw =
        floors || (data_dropped != command && download_then_run(&data_dropped, true)?);
    let mut out: Vec<String> = Vec::new();
    let mut candidates = vec![data_dropped, code_dropped];
    if include_raw {
        candidates.push(command.to_owned());
    }
    for source in candidates {
        if !out.contains(&source) {
            out.push(source);
        }
    }
    Ok(out)
}

struct Seen(HashSet<String>);

impl Seen {
    fn fresh(&mut self, variant: &str) -> bool {
        !variant.is_empty() && self.0.insert(variant.to_owned())
    }
}

fn has_windows_path(command: &str) -> bool {
    static A: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:[A-Za-z]:|\\\\)[\\\\]").expect("valid pattern"));
    static B: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[A-Za-z]:\\").expect("valid pattern"));
    A.is_match(command) || B.is_match(command)
}

fn mark(command: &str, marker: &str, skip: &[(usize, usize)]) -> R<String> {
    mark_command_starts(&Text::new(command), marker, skip)
}

fn single_command_detection_variants(
    command: &str,
    eval_depth: usize,
    capped: &mut Vec<String>,
    env: &HomeEnv,
    floors: bool,
) -> R<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let command = expand_ansi_c_quotes(command);
    let normalized = normalize_command_for_detection(&mask_quoted_newlines(&command)?, env);
    let (grep_safe, _) = grep_safe_detection_variant(&normalized)?;
    let mut seen = Seen(HashSet::new());
    seen.0.insert(grep_safe.clone());
    out.push(grep_safe.clone());

    let mut pending: Vec<(String, usize)> = vec![(normalized.clone(), eval_depth)];
    if has_windows_path(&command) {
        let win_variant = normalize_command_for_detection(
            &mask_quoted_newlines(&command.replace('\\', "/"))?,
            env,
        );
        if seen.fresh(&win_variant) {
            out.push(win_variant.clone());
            pending.push((win_variant, eval_depth));
        }
        let marked_win = normalize_command_for_detection(
            &mark(&mask_quoted_newlines(&command)?, " \n", &[])?.replace('\\', "/"),
            env,
        );
        if seen.fresh(&marked_win) {
            out.push(marked_win);
        }
    }
    while let Some((source, depth)) = pending.pop() {
        for (payload, payload_depth) in nested_payloads(&source, depth, Some(capped))? {
            if seen.fresh(&payload) {
                out.push(payload.clone());
                let marked_payload = mark(&payload, "\n", &[])?;
                if marked_payload != payload && seen.fresh(&marked_payload) {
                    out.push(marked_payload);
                }
                pending.push((payload, payload_depth));
            }
        }
    }
    if !capped.is_empty() {
        return Ok(out);
    }
    for (payload, payload_depth) in
        nested_payloads(&mask_quoted_newlines(&command)?, eval_depth, Some(capped))?
    {
        if payload != command {
            for variant in command_detection_variants(&payload, payload_depth, capped, env, floors)?
            {
                if seen.fresh(&variant) {
                    out.push(variant);
                }
            }
        }
    }
    let marked = mark(&grep_safe, "\n", &[])?;
    if marked != grep_safe && seen.fresh(&marked) {
        out.push(marked);
    }
    let faithful =
        normalize_command_for_detection(&mark(&mask_quoted_newlines(&command)?, " \n", &[])?, env);
    if seen.fresh(&faithful) {
        out.push(faithful);
    }
    let joined = join_escaped_word_chars(&command);
    if joined != command {
        let variant = normalize_command_for_detection(&mask_quoted_newlines(&joined)?, env);
        if seen.fresh(&variant) {
            out.push(variant);
        }
    }
    let normalized_text = Text::new(&normalized);
    let mut pending_spans: Vec<Span> = Vec::new();
    for (start, end, word) in iter_shell_command_word_spans(&normalized_text)? {
        let deobfuscated = deobfuscate_shell_word_for_detection(&word)?;
        if !deobfuscated.is_empty() && deobfuscated != word {
            pending_spans.push((start, end, deobfuscated));
        }
    }
    pending_spans.sort_by_key(|(s, e, _)| (*s, *e));
    while !pending_spans.is_empty() {
        let (mut applied, mut carry): (Vec<Span>, Vec<Span>) = (Vec::new(), Vec::new());
        let mut cursor = 0;
        for span in pending_spans {
            if span.0 < cursor {
                carry.push(span);
            } else {
                cursor = span.1;
                applied.push(span);
            }
        }
        let edits: Vec<(usize, usize, &str)> = applied
            .iter()
            .map(|(s, e, t)| (*s, *e, t.as_str()))
            .collect();
        let variant = splice(&normalized_text.chars, &edits);
        let marked = mark(&variant, "\n", &literal_heredoc_bodies(&variant))?;
        for candidate in [variant, marked] {
            if seen.fresh(&candidate) {
                out.push(candidate);
            }
        }
        pending_spans = carry;
    }
    Ok(out)
}

/// Variants of the command with provably inert heredoc bodies dropped; the floors also scan the raw one.
pub fn command_detection_variants(
    command: &str,
    eval_depth: usize,
    capped: &mut Vec<String>,
    env: &HomeEnv,
    floors: bool,
) -> R<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for source in heredoc_scan_sources(command, floors)? {
        for variant in single_command_detection_variants(&source, eval_depth, capped, env, floors)?
        {
            if seen.insert(variant.clone()) {
                out.push(variant);
            }
        }
    }
    Ok(out)
}

/// Detection variants for literal carried command strings, recursing a bounded depth.
fn carried_command_variants(
    command: &str,
    capped: &mut Vec<String>,
    env: &HomeEnv,
) -> R<Vec<String>> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([command.to_owned()]);
    let mut frontier = vec![command.to_owned()];
    for _ in 0..MAX_CARRIER_DEPTH {
        let mut nested = Vec::new();
        for source in &frontier {
            for payload in carried_payloads(source)? {
                if !seen.insert(payload.clone()) {
                    continue;
                }
                out.extend(command_detection_variants(&payload, 0, capped, env, true)?);
                let payload = blank_literal_heredoc_bodies(&payload);
                nested.push(payload.clone());
                let peeled = strip_wrapper_prefixes(&payload)?;
                out.extend(command_detection_variants(&peeled, 0, capped, env, true)?);
                if seen.insert(peeled.clone()) {
                    nested.push(peeled);
                }
            }
        }
        frontier = nested;
    }
    Ok(out)
}

/// The shared detection variants plus the hardline-only command-position projections.
pub fn hardline_detection_variants(
    command: &str,
    capped: &mut Vec<String>,
    env: &HomeEnv,
) -> R<Vec<String>> {
    let masked = blank_literal_heredoc_bodies(&mask_quoted_newlines(command)?);
    let stripped = strip_wrapper_prefixes(&masked)?;
    let mut all = command_detection_variants(command, 0, capped, env, true)?;
    if stripped != masked {
        all.extend(command_detection_variants(&stripped, 0, capped, env, true)?);
        all.extend(carried_command_variants(&stripped, capped, env)?);
    }
    all.extend(carried_command_variants(
        &blank_literal_heredoc_bodies(command),
        capped,
        env,
    )?);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for variant in all {
        let dequoted = if variant.to_lowercase().contains("rm") {
            dequote_rm_operands(&variant)?
        } else {
            variant.clone()
        };
        for candidate in [variant, dequoted] {
            if seen.insert(candidate.clone()) {
                out.push(candidate);
            }
        }
    }
    Ok(out)
}

/// Outcome of the hardline scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardlineVerdict {
    pub blocked: bool,
    pub description: Option<String>,
    /// False when the scan ran out of its step budget before reading all of the command.
    pub complete: bool,
}

fn detect_inner(command: &str, env: &HomeEnv) -> R<(bool, Option<String>)> {
    if command_parser_limit_exceeded(command) {
        return Ok((true, Some(PARSER_LIMIT_DESCRIPTION.into())));
    }
    for source in heredoc_scan_sources(command, true)? {
        let masked = mask_quoted_newlines(&expand_ansi_c_quotes(&source))?;
        if grep_safe_detection_variant(&masked)?.1 {
            return Ok((true, Some(MALFORMED_EXEC_DESCRIPTION.into())));
        }
    }
    let patterns = if command.to_lowercase().contains("whatif") && !whatif_preview_is_real(command)?
    {
        &*PATTERNS_NO_WHATIF
    } else {
        &*PATTERNS
    };
    let mut capped = Vec::new();
    for variant in hardline_detection_variants(command, &mut capped, env)? {
        let lower = variant.to_lowercase();
        let mut masked_lower: Option<String> = None;
        for pattern in patterns {
            if pattern.quote_masked && masked_lower.is_none() {
                masked_lower = Some(if contains_shell_carrier(&variant)? {
                    lower.clone()
                } else {
                    mask_quoted_prose(&variant)?.to_lowercase()
                });
            }
            let subject = if pattern.quote_masked {
                masked_lower.as_deref().unwrap_or("")
            } else {
                lower.as_str()
            };
            if search(&pattern.regex, subject)? {
                return Ok((true, Some(pattern.description.into())));
            }
        }
    }
    Ok(if capped.is_empty() {
        (false, None)
    } else {
        (true, Some(PARSER_LIMIT_DESCRIPTION.into()))
    })
}

/// A linear over-approximation of the hardline rule a budget-tripped command may hold.
fn hardline_fallback(command: &str, env: &HomeEnv) -> Option<&'static str> {
    static COMMAND_START: LazyLock<Regex> = LazyLock::new(|| {
        compile(
            r#"\$\(|[;&|(){}`'"\n]|(?<![\w./-])(?:if|then|else|elif|do|while|until|!)(?=\s)"#,
            "",
        )
    });
    static UNQUOTED_START: LazyLock<Regex> = LazyLock::new(|| {
        compile(
            r"\$\(|[;&|()`\n]|(?<!\$)\{|\}|(?<![\w./-])(?:if|then|else|elif|do|while|until|!)(?=\s)",
            "",
        )
    });
    let texts = [
        command.to_owned(),
        expand_ansi_c_quotes(command),
        normalize_command_for_detection(command, env),
    ];
    for text in texts {
        let lowered = text.to_lowercase();
        let unquoted = lowered.replace(['\'', '"'], "");
        let variants = [
            lowered.clone(),
            COMMAND_START.replace_all(&lowered, "\n").into_owned(),
            unquoted.clone(),
            UNQUOTED_START.replace_all(&unquoted, "\n").into_owned(),
        ];
        for variant in &variants {
            for pattern in PATTERNS.iter() {
                if pattern.regex.is_match(variant).unwrap_or(false) {
                    return Some(pattern.description);
                }
            }
        }
    }
    None
}

/// Check the hardline patterns (never bypassable, even in YOLO).
pub fn detect_hardline_command(command: &str, env: &HomeEnv) -> HardlineVerdict {
    let len = command.chars().count();
    take_recursion_limit();
    match with_budget(len, || detect_inner(command, env)) {
        Ok((blocked, description)) => HardlineVerdict {
            blocked,
            description,
            complete: true,
        },
        Err(_) if take_recursion_limit() => HardlineVerdict {
            blocked: true,
            description: Some(PARSER_LIMIT_DESCRIPTION.into()),
            complete: true,
        },
        Err(_) => match hardline_fallback(command, env) {
            Some(description) => HardlineVerdict {
                blocked: true,
                description: Some(format!("{description}{FALLBACK_MATCH_SUFFIX}")),
                complete: false,
            },
            None => HardlineVerdict {
                blocked: false,
                description: None,
                complete: false,
            },
        },
    }
}

pub const SUDO_STDIN_DESCRIPTION: &str = "sudo password guessing via stdin (sudo -S)";

/// `sudo -S` typed by the model without a configured `SUDO_PASSWORD`: a password piped through stdin
/// is a guessing vector, refused unconditionally (`_check_sudo_stdin_guard`). The reference lowercases
/// before an ignore-case match, so a plain `sudo -s` is refused as well.
pub fn check_sudo_stdin_guard(command: &str, env: &HomeEnv) -> Option<&'static str> {
    static SUDO_STDIN: LazyLock<Regex> =
        LazyLock::new(|| compile(r"(?:^|[;&|`\n]|&&|\|\||\$\()\s*sudo\s+-S\b", "(?i)"));
    if std::env::var_os("SUDO_PASSWORD").is_some() {
        return None;
    }
    let normalized = normalize_command_for_detection(command, env).to_lowercase();
    SUDO_STDIN
        .is_match(&normalized)
        .unwrap_or(false)
        .then_some(SUDO_STDIN_DESCRIPTION)
}
