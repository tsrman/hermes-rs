//! Where a command may start, and which words can be executable names
//! (`_iter_shell_command_starts`, `_mark_command_starts`, `_iter_shell_command_word_spans`,
//! `_shell_command_segment`). The reference yields these lazily; here they are collected, so a
//! caller that would have stopped early spends the budget of the whole scan.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ansi_c::is_shell_comment_start;
use super::budget::{BudgetExceeded, Text};
use super::py_isspace;
use super::shell_scan::{scan_shell, Kind, ScanOpts};
use super::shell_words::{
    deobfuscate_shell_word_for_detection, read_shell_word, skip_shell_whitespace,
};
use super::wrappers::{
    command_wrapper_options_with_arg, is_command_wrapper_word, is_query_option,
    non_executing_options, positional_args, SHELL_COMMAND_TRANSITIONS,
};

fn scan_starts(
    text: &Text,
    start: usize,
    end: usize,
    starts: &mut Vec<usize>,
) -> Result<(), BudgetExceeded> {
    let chars = &text.chars;
    let opts = ScanOpts {
        subst_unquoted: true,
        subst_quoted: true,
        stop_unterminated: true,
        comments: true,
        ..ScanOpts::default()
    };
    let mut skip: Option<usize> = None;
    for step in scan_shell(text, start, Some(end), opts) {
        let step = step?;
        match step.kind {
            Kind::Subst => {
                let inner = step.i + if chars[step.i] == '`' { 1 } else { 2 };
                starts.push(inner);
                scan_starts(text, inner, step.j.map_or(end, |j| j - 1), starts)?;
            }
            Kind::Char if step.quote.is_none() && Some(step.i) != skip => {
                let (i, c) = (step.i, chars[step.i]);
                let brace_group = c == '{'
                    && (i == 0 || py_isspace(chars[i - 1]) || "(;&|)".contains(chars[i - 1]));
                if "(;\n".contains(c) || brace_group {
                    starts.push(i + 1);
                } else if c == '&' || c == '|' {
                    let repeated = i + 1 < end && chars[i + 1] == c;
                    if repeated {
                        skip = Some(i + 1);
                    }
                    starts.push(i + 1 + usize::from(repeated));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Offsets where a command may start, in the order the reference yields them.
pub fn iter_shell_command_starts(text: &Text) -> Result<Vec<usize>, BudgetExceeded> {
    let chars = &text.chars;
    let mut starts = vec![0usize];
    scan_starts(text, 0, text.len(), &mut starts)?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let (mut run_from, mut run_to) = (usize::MAX, 0usize);
    let mut index = 0;
    while index < starts.len() {
        let original = starts[index];
        index += 1;
        if !(run_from != usize::MAX && run_from <= original && original <= run_to) {
            run_from = original;
            run_to = skip_shell_whitespace(chars, original);
        }
        let start = run_to;
        if start >= chars.len() || seen.contains(&start) || is_shell_comment_start(chars, start) {
            continue;
        }
        seen.insert(start);
        out.push(start);
        let (_, end, word) = read_shell_word(text, start)?;
        if SHELL_COMMAND_TRANSITIONS.contains(&word.as_str()) {
            starts.push(end);
        }
    }
    Ok(out)
}

/// Apply sorted, non-overlapping `(start, end, text)` edits in one pass.
pub fn splice(chars: &[char], edits: &[(usize, usize, &str)]) -> String {
    let mut out = String::new();
    let mut previous = 0;
    for &(start, end, text) in edits {
        out.extend(&chars[previous.min(start)..start]);
        out.push_str(text);
        previous = end;
    }
    out.extend(&chars[previous.min(chars.len())..]);
    out
}

/// `command` with `marker` inserted before each real (quote-aware) command start; starts inside a
/// `skip` span (sorted, non-overlapping) are left alone.
pub fn mark_command_starts(
    text: &Text,
    marker: &str,
    skip: &[(usize, usize)],
) -> Result<String, BudgetExceeded> {
    let mut offsets: Vec<usize> = iter_shell_command_starts(text)?
        .into_iter()
        .filter(|&o| {
            o > 0 && {
                let k = skip.partition_point(|&(a, _)| a <= o);
                !(k > 0 && o < skip[k - 1].1)
            }
        })
        .collect();
    offsets.sort_unstable();
    if offsets.is_empty() {
        return Ok(text.chars.iter().collect());
    }
    let edits: Vec<(usize, usize, &str)> = offsets.iter().map(|&o| (o, o, marker)).collect();
    Ok(splice(&text.chars, &edits))
}

/// `_SHELL_REDIRECTION_RE.match` at `pos`: `[0-9]*` then an operator; returns the end offset.
fn match_redirection(chars: &[char], pos: usize) -> Option<usize> {
    let mut k = pos;
    while chars.get(k).is_some_and(char::is_ascii_digit) {
        k += 1;
    }
    let two = |a: char, b: char| chars.get(k) == Some(&a) && chars.get(k + 1) == Some(&b);
    if two('>', '>')
        || two('<', '<')
        || two('<', '>')
        || two('>', '&')
        || two('<', '&')
        || two('>', '|')
    {
        return Some(k + 2);
    }
    matches!(chars.get(k), Some('<' | '>')).then_some(k + 1)
}

fn basename_lower(word: &str) -> String {
    word.rsplit('/').next().unwrap_or("").to_lowercase()
}

fn is_env_assignment(word: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A[A-Za-z_][A-Za-z0-9_]*=[^\n]*\z").expect("valid pattern"));
    RE.is_match(word)
}

/// Command-position words that may be executable names: `(start, end, word)`.
pub fn iter_shell_command_word_spans(
    text: &Text,
) -> Result<Vec<(usize, usize, String)>, BudgetExceeded> {
    let chars = &text.chars;
    let mut out = Vec::new();
    for mut pos in iter_shell_command_starts(text)? {
        let mut wrapper: Option<String> = None;
        let mut positionals = 0usize;
        let (mut options, mut skip_arg) = (true, false);
        while pos < chars.len() {
            if let Some(end) = match_redirection(chars, skip_shell_whitespace(chars, pos)) {
                pos = read_shell_word(text, end)?.1;
                continue;
            }
            let (word_start, word_end, word) = read_shell_word(text, pos)?;
            if word_start == word_end {
                break;
            }
            pos = word_end;
            let deobfuscated = deobfuscate_shell_word_for_detection(&word)?;
            let name = basename_lower(&deobfuscated);
            if skip_arg {
                skip_arg = false;
                continue;
            }
            if let Some(w) = wrapper.as_deref() {
                if options && deobfuscated == "--" {
                    options = false;
                    continue;
                }
                if options && deobfuscated.starts_with('-') {
                    let option = deobfuscated.split('=').next().unwrap_or("");
                    if w == "env" && (option == "--split-string" || deobfuscated.starts_with("-S"))
                    {
                        break;
                    }
                    let command_query = w == "command"
                        && !option.starts_with("--")
                        && option.chars().skip(1).any(|c| c == 'v' || c == 'V');
                    if non_executing_options(w).contains(&option)
                        || command_query
                        || (w == "sudo" && is_query_option("sudo", option))
                    {
                        break;
                    }
                    skip_arg = !deobfuscated.contains('=')
                        && command_wrapper_options_with_arg(w).contains(&option);
                    continue;
                }
            }
            if positionals > 0 {
                positionals -= 1;
                continue;
            }
            if is_env_assignment(&word) {
                continue;
            }
            out.push((word_start, word_end, word));
            if !is_command_wrapper_word(&name) {
                break;
            }
            positionals = positional_args(&name);
            wrapper = Some(name);
            options = true;
        }
    }
    Ok(out)
}

/// Bound a candidate to its command, keeping quoted bytes. The `&`/`|` of `2>&1`, `<&3`, `>|`
/// and `&>` belong to the command, not to a separator.
pub fn shell_command_segment(text: &Text, start: usize) -> Result<String, BudgetExceeded> {
    let chars = &text.chars;
    let mut end = chars.len();
    let mut redirect_end: Option<usize> = None;
    let opts = ScanOpts {
        subst_unquoted: true,
        subst_quoted: true,
        brace: true,
        comments: true,
        ..ScanOpts::default()
    };
    for step in scan_shell(text, start, None, opts) {
        let step = step?;
        let j = step.j.unwrap_or(step.i + 1);
        let c = chars[step.i];
        let plain = step.kind == Kind::Char && step.quote.is_none();
        if plain
            && ((c == '&' && (redirect_end == Some(step.i) || chars.get(j) == Some(&'>')))
                || (c == '|'
                    && redirect_end == Some(step.i)
                    && step.i > 0
                    && chars[step.i - 1] == '>'))
        {
            continue;
        }
        redirect_end = (plain && (c == '<' || c == '>')).then_some(j);
        if step.kind == Kind::Comment || (plain && ";&|\n)`".contains(c)) {
            end = step.i;
            break;
        }
    }
    let segment: String = chars[start.min(end)..end].iter().collect();
    Ok(segment.trim_matches(py_isspace).to_owned())
}
