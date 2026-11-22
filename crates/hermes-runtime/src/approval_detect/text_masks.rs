//! Quoted-newline masking, escaped-separator joining and the parser size limits
//! (`approval_quoted_newlines.py`, `_join_escaped_word_chars`, `_command_parser_limit_exceeded`).

use std::sync::LazyLock;

use fancy_regex::Regex as FancyRegex;
use regex::Regex;

use super::budget::{BudgetExceeded, Text};
use super::normalize::nfkc;
use super::shell_scan::{scan_shell, substitution_depth, Kind, ScanOpts};

fn mask_span(text: &Text, start: usize, end: usize) -> Result<String, BudgetExceeded> {
    let chars = &text.chars;
    let mut out = String::new();
    let opts = ScanOpts {
        subst_quoted: true,
        comments: true,
        ..ScanOpts::default()
    };
    for step in scan_shell(text, start, Some(end), opts) {
        let step = step?;
        let j = step.j.unwrap_or(step.i + 1);
        if step.kind == Kind::Subst {
            let body_start = step.i
                + if chars[step.i] == '$' && chars.get(step.i + 1) == Some(&'(') {
                    2
                } else {
                    1
                };
            let body_end = j - 1;
            out.extend(&chars[step.i..body_start]);
            out.push_str(&mask_span(text, body_start, body_end)?);
            out.extend(&chars[body_end..j]);
        } else if step.quote.is_some()
            && step.kind == Kind::Char
            && matches!(chars[step.i], '\r' | '\n')
        {
            out.push(' ');
        } else {
            out.extend(&chars[step.i..j]);
        }
    }
    Ok(out)
}

/// Replace raw newlines (and CRs) inside quotes with a space; a `$(...)` inside double quotes is
/// executable, so its body is rescanned with a fresh quote state.
pub fn mask_quoted_newlines(command: &str) -> Result<String, BudgetExceeded> {
    if !command.contains('\n') && !command.contains('\r') {
        return Ok(command.to_owned());
    }
    let text = Text::new(command);
    mask_span(&text, 0, text.len())
}

/// Turn each escaped `;`/`&`/`|`, space, tab or quote into `_`, so it stays literal text inside
/// its word once normalization strips the backslash.
pub fn join_escaped_word_chars(command: &str) -> String {
    static SPAN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"'[^']*'|"(?:[^"\\]|\\[\s\S])*"|\\[\s\S]"#).expect("valid pattern")
    });
    SPAN.replace_all(command, |caps: &regex::Captures| {
        let span = &caps[0];
        let joinable = if span.starts_with('\'') {
            ";&|"
        } else {
            ";&| \t\"'`"
        };
        let mut out = String::new();
        let mut it = span.chars().peekable();
        while let Some(c) = it.next() {
            match (c, it.peek().copied()) {
                ('\\', Some(next)) if next != '\n' => {
                    it.next();
                    if joinable.contains(next) {
                        out.push('_');
                    } else {
                        out.push('\\');
                        out.push(next);
                    }
                }
                _ => out.push(c),
            }
        }
        out
    })
    .into_owned()
}

pub const MAX_DETECTION_COMMAND_CHARS: usize = 128_000;
pub const MAX_SEPARATOR_FREE_COMMAND_CHARS: usize = 4_096;
pub const MAX_DETECTION_SEGMENTS: usize = 25_000;
pub const MAX_SUBSTITUTION_DEPTH: usize = 32;

/// Bound parser work before normalization: size, separator-free size, `$(` nesting, segment count.
/// Crossing a ceiling fails closed (the caller refuses).
pub fn command_parser_limit_exceeded(command: &str) -> bool {
    static CONTINUATION: LazyLock<FancyRegex> =
        LazyLock::new(|| FancyRegex::new(r"(?<!\\)((?:\\\\)*+)\\\r?\n").expect("valid pattern"));
    let len = command.chars().count();
    if len > MAX_DETECTION_COMMAND_CHARS {
        return true;
    }
    let is_sep = |c: char| matches!(c, ';' | '&' | '|' | '\n');
    if len > MAX_SEPARATOR_FREE_COMMAND_CHARS && !command.chars().any(is_sep) {
        return true;
    }
    let folded = nfkc(command);
    let stripped = CONTINUATION.replace_all(&folded, "$1").into_owned();
    if substitution_depth(&folded).max(substitution_depth(&stripped)) > MAX_SUBSTITUTION_DEPTH {
        return true;
    }
    command.chars().filter(|c| is_sep(*c)).count() >= MAX_DETECTION_SEGMENTS
}
