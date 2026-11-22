//! Top-level segments, the span-keeping lexer and quoted grep operands
//! (`_iter_top_level_shell_segments`, `approval_shell_tokens.py`, `approval_grep_spans.py`).

use super::budget::{BudgetExceeded, Text};
use super::command_starts::{iter_shell_command_word_spans, splice};
use super::py_isspace;
use super::shell_scan::{scan_shell, Kind, ScanOpts};
use super::shell_words::{deobfuscate_shell_word_for_detection, shlex_split_punct};

/// Top-level command segments in one left-to-right pass (comments and `;&|\n` split).
pub fn iter_top_level_shell_segments(command: &str) -> Result<Vec<String>, BudgetExceeded> {
    let text = Text::new(command);
    let chars = &text.chars;
    let opts = ScanOpts {
        comments: true,
        ..ScanOpts::default()
    };
    let mut out = Vec::new();
    let mut start = 0;
    for step in scan_shell(&text, 0, None, opts) {
        let step = step?;
        let c = chars[step.i];
        if step.kind == Kind::Comment
            || (step.kind == Kind::Char && step.quote.is_none() && ";&|\n".contains(c))
        {
            if start < step.i {
                out.push(chars[start..step.i].iter().collect());
            }
            start = step.j.unwrap_or(step.i + 1);
        }
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    Ok(out)
}

/// Tokenize an already-bounded segment from `start`; `None` is malformed quoting.
pub fn shell_segment_tokens(segment: &str, start: usize) -> Option<Vec<String>> {
    let rest: String = segment.chars().skip(start).collect();
    shlex_split_punct(&rest)
}

/// One shell word with its source span and whether it is wholly single-quoted (inert data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanToken {
    pub value: String,
    pub start: usize,
    pub end: usize,
    pub inert: bool,
}

fn backtick_end_from(chars: &[char], i: usize) -> Option<usize> {
    let mut j = (i + 1..chars.len()).find(|&k| chars[k] == '`');
    while let Some(k) = j {
        if chars[k - 1] != '\\' {
            return Some(k);
        }
        j = (k + 1..chars.len()).find(|&m| chars[m] == '`');
    }
    None
}

/// Shell words as `(value, start, end, quoted)`; `None` on malformed quoting. Lexing stops at the
/// end of the simple command that begins at `start`.
pub fn shell_tokens_with_spans(
    text: &Text,
    start: usize,
) -> Result<Option<Vec<SpanToken>>, BudgetExceeded> {
    let chars = &text.chars;
    let mut tokens = Vec::new();
    let mut value = String::new();
    let mut token_start: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut in_backtick = false;
    let mut end_at = chars.len();
    let flush =
        |tokens: &mut Vec<SpanToken>, value: &mut String, token_start: usize, end: usize| {
            let raw: String = chars[token_start..end].iter().collect();
            let inert = (raw.starts_with('\'') && raw.ends_with('\''))
                || (raw.contains("='") && raw.ends_with('\''));
            tokens.push(SpanToken {
                value: std::mem::take(value),
                start: token_start,
                end,
                inert,
            });
        };
    for step in scan_shell(text, start, None, ScanOpts::default()) {
        let step = step?;
        let (i, ch) = (step.i, chars[step.i]);
        if step.kind == Kind::Char && quote.is_none() {
            if py_isspace(ch) && ch != '\n' {
                if let Some(ts) = token_start {
                    flush(&mut tokens, &mut value, ts, i);
                    token_start = None;
                }
                continue;
            }
            if ch == '$' && chars.get(i + 1) == Some(&'(') {
                depth += 1;
            } else if ch == '`' {
                if in_backtick {
                    in_backtick = false;
                } else if depth == 0 && backtick_end_from(chars, i).is_none() {
                    end_at = i;
                    break;
                } else {
                    in_backtick = true;
                }
            } else if ch == ')' {
                if depth == 0 {
                    end_at = i;
                    break;
                }
                depth -= 1;
            } else if ";|&\n".contains(ch) {
                end_at = i;
                break;
            }
        }
        if token_start.is_none() {
            token_start = Some(i);
        }
        if step.kind == Kind::Quote {
            quote = if quote.is_some() { None } else { Some(ch) };
        } else if step.kind == Kind::Esc {
            value.push(chars[i + 1]);
        } else if ch == '\\' && quote.is_none() {
            return Ok(None);
        } else {
            value.push(ch);
        }
    }
    if quote.is_some() {
        return Ok(None);
    }
    if let Some(ts) = token_start {
        flush(&mut tokens, &mut value, ts, end_at);
    }
    Ok(Some(tokens))
}

const GREP_OPTIONS_WITH_ARG: [&str; 14] = [
    "--after-context",
    "--before-context",
    "--binary-files",
    "--context",
    "--directories",
    "--devices",
    "--exclude",
    "--exclude-dir",
    "--exclude-from",
    "--include",
    "--label",
    "--max-count",
    "--regexp",
    "--file",
];
const GREP_SHORT_OPTIONS_WITH_ARG: [char; 8] = ['A', 'B', 'C', 'D', 'd', 'e', 'f', 'm'];

fn find_sub(chars: &[char], pattern: &[char], from: usize) -> Option<usize> {
    if pattern.is_empty() {
        return Some(from.min(chars.len()));
    }
    (from..chars.len().saturating_sub(pattern.len() - 1))
        .find(|&k| chars[k..k + pattern.len()] == *pattern)
}

/// Locate quoted grep PCRE operands: `(spans, malformed)`. On an ambiguous parse nothing is hidden.
pub fn quoted_grep_pattern_spans(
    command: &str,
) -> Result<(Vec<(usize, usize)>, bool), BudgetExceeded> {
    let command_chars: Vec<char> = command.chars().collect();
    let mut spans = Vec::new();
    let mut offset = 0;
    for segment in iter_top_level_shell_segments(command)? {
        let segment_chars: Vec<char> = segment.chars().collect();
        let segment_at = find_sub(&command_chars, &segment_chars, offset).unwrap_or(offset);
        offset = segment_at + segment_chars.len();
        let seg_text = Text::new(&segment);
        for (start, _, word) in iter_shell_command_word_spans(&seg_text)? {
            let name = deobfuscate_shell_word_for_detection(&word)?;
            let base = name.rsplit('/').next().unwrap_or("").to_lowercase();
            if base != "grep" && base != "egrep" {
                continue;
            }
            let Some(tokens) = shell_tokens_with_spans(&seg_text, start)? else {
                return Ok((Vec::new(), true));
            };
            let args = &tokens[1.min(tokens.len())..];
            let mut pattern_indexes: Vec<usize> = Vec::new();
            let (mut pcre, mut explicit_patterns) = (false, false);
            let mut operand_index: Option<usize> = None;
            let (mut i, mut options) = (0usize, true);
            while i < args.len() {
                let token = args[i].value.as_str();
                if options && token == "--" {
                    options = false;
                } else if options && token.starts_with("--") {
                    let (option, equals) = match token.split_once('=') {
                        Some((o, _)) => (o, true),
                        None => (token, false),
                    };
                    pcre = pcre || option == "--perl-regexp";
                    explicit_patterns =
                        explicit_patterns || option == "--regexp" || option == "--file";
                    let takes_next = GREP_OPTIONS_WITH_ARG.contains(&option) && !equals;
                    if takes_next && i + 1 >= args.len() {
                        return Ok((Vec::new(), true));
                    }
                    if option == "--regexp" {
                        pattern_indexes.push(if takes_next { i + 1 } else { i });
                    }
                    if takes_next {
                        i += 1;
                    }
                } else if options && token.starts_with('-') && token != "-" {
                    let chars: Vec<char> = token.chars().skip(1).collect();
                    for (j, c) in chars.iter().enumerate() {
                        pcre = pcre || *c == 'P';
                        explicit_patterns = explicit_patterns || *c == 'e' || *c == 'f';
                        if GREP_SHORT_OPTIONS_WITH_ARG.contains(c) {
                            let attached = j + 1 < chars.len();
                            if !attached && i + 1 >= args.len() {
                                return Ok((Vec::new(), true));
                            }
                            if *c == 'e' {
                                pattern_indexes.push(if attached { i } else { i + 1 });
                            }
                            if !attached {
                                i += 1;
                            }
                            break;
                        }
                    }
                } else if operand_index.is_none() {
                    operand_index = Some(i);
                }
                i += 1;
            }
            if !explicit_patterns {
                match operand_index {
                    None => return Ok((Vec::new(), pcre)),
                    Some(k) => pattern_indexes.push(k),
                }
            }
            if pcre {
                for k in pattern_indexes {
                    let token = &args[k];
                    if token.inert {
                        spans.push((segment_at + token.start, segment_at + token.end));
                    }
                }
            }
        }
    }
    Ok((spans, false))
}

/// `command` with quoted grep PCRE operands blanked; `(variant, malformed)`.
pub fn grep_safe_detection_variant(command: &str) -> Result<(String, bool), BudgetExceeded> {
    let (spans, malformed) = quoted_grep_pattern_spans(command)?;
    if malformed || spans.is_empty() {
        return Ok((command.to_owned(), malformed));
    }
    let blanks: Vec<String> = spans.iter().map(|(s, e)| " ".repeat(e - s)).collect();
    let edits: Vec<(usize, usize, &str)> = spans
        .iter()
        .zip(&blanks)
        .map(|((s, e), b)| (*s, *e, b.as_str()))
        .collect();
    let chars: Vec<char> = command.chars().collect();
    Ok((splice(&chars, &edits), false))
}
