//! Wrapper peeling, literal heredoc bodies and `rm` operand dequoting (`approval_wrappers.py`).

use std::sync::LazyLock;

use fancy_regex::Regex as FancyRegex;
use regex::Regex;

use super::budget::{BudgetExceeded, Text};
use super::command_starts::{iter_shell_command_starts, splice};
use super::patterns::RM_FLAG_PREFIX;
use super::py_isspace;
use super::shell_scan::{scan_shell, Kind, ScanOpts};
use super::shell_words::{
    deobfuscate_shell_word_for_detection, read_shell_word, strip_shell_word_syntax,
};
use super::wrappers::{
    is_env_split_option, is_query_option, long_option, option_kind, PEEL_WRAPPER_WORDS,
};

fn is_env_assignment(word: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A[A-Za-z_][A-Za-z0-9_]*=[^\n]*\z").expect("valid pattern"));
    RE.is_match(word)
}

fn duration_word(word: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\A\+?(?:(?i:inf(?:inity)?)|0[xX][0-9a-fA-F]+|(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)[smhd]?\z",
        )
        .expect("valid pattern")
    });
    RE.is_match(word)
}

fn number_word(word: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A[+-]?\d+\z").expect("valid pattern"));
    RE.is_match(word)
}

/// `_LEADING_REDIRECT_RE.match` at `pos`: blanks, `\d+|&`, an operator, not followed by `<` or `(`.
fn leading_redirect_end(chars: &[char], pos: usize) -> Option<usize> {
    let mut k = pos;
    while chars.get(k).is_some_and(|c| py_isspace(*c) && *c != '\n') {
        k += 1;
    }
    if chars.get(k).is_some_and(char::is_ascii_digit) {
        while chars.get(k).is_some_and(char::is_ascii_digit) {
            k += 1;
        }
    } else if chars.get(k) == Some(&'&') {
        k += 1;
    }
    for op in [">>", ">&", ">|", "<>", "<&", "<<<", ">", "<"] {
        let n = op.chars().count();
        let matches = op
            .chars()
            .enumerate()
            .all(|(o, c)| chars.get(k + o) == Some(&c));
        if matches && !matches!(chars.get(k + n), Some('<' | '(')) {
            return Some(k + n);
        }
    }
    None
}

fn skip_redirects(text: &Text, mut pos: usize) -> Result<usize, BudgetExceeded> {
    while let Some(end) = leading_redirect_end(&text.chars, pos) {
        let (ws, we, _) = read_shell_word(text, end)?;
        if ws == we {
            return Ok(pos);
        }
        pos = we;
    }
    Ok(pos)
}

fn timeout_duration_end(text: &Text, pos: usize) -> Result<usize, BudgetExceeded> {
    let (_, end, word) = read_shell_word(text, pos)?;
    Ok(if duration_word(&word) { end } else { pos })
}

fn base_name(word: &str) -> String {
    word.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
}

/// Offset where the real program after any leading wrappers begins (`start` when none).
fn wrapper_prefix_end(text: &Text, start: usize) -> Result<usize, BudgetExceeded> {
    let (mut pos, mut seen_prefix) = (start, false);
    loop {
        let after = skip_redirects(text, pos)?;
        if after != pos {
            pos = after;
            seen_prefix = true;
        }
        let (ws, we, word) = read_shell_word(text, pos)?;
        if ws == we {
            return Ok(start);
        }
        if is_env_assignment(&word) {
            pos = we;
            seen_prefix = true;
            continue;
        }
        let base = base_name(&deobfuscate_shell_word_for_detection(&word)?);
        if base == "busybox" || base == "toybox" {
            let (nws, nwe, nword) = read_shell_word(text, we)?;
            if nws == nwe || nword.starts_with('-') {
                return Ok(if seen_prefix { ws } else { start });
            }
            seen_prefix = true;
            pos = we;
            continue;
        }
        if !PEEL_WRAPPER_WORDS.contains(&base.as_str()) {
            return Ok(if seen_prefix { ws } else { start });
        }
        let wrapper_start = if seen_prefix { ws } else { start };
        seen_prefix = true;
        pos = we;
        loop {
            pos = skip_redirects(text, pos)?;
            let (ows, owe, oword) = read_shell_word(text, pos)?;
            let oword = long_option(&base, &oword);
            if ows == owe || oword == "--" {
                pos = if base == "timeout" {
                    timeout_duration_end(text, owe)?
                } else {
                    owe
                };
                break;
            }
            if oword.starts_with('-') && oword != "-" {
                if is_query_option(&base, &oword) {
                    return Ok(start);
                }
                if is_env_split_option(&base, &oword) {
                    return Ok(wrapper_start);
                }
                if oword.contains('=') {
                    pos = owe;
                    continue;
                }
                let Some(kind) = option_kind(&base, &oword) else {
                    pos = owe;
                    continue;
                };
                let (vws, vwe, value) = read_shell_word(text, owe)?;
                if vws == vwe {
                    pos = owe;
                    continue;
                }
                if kind == "num" && !number_word(&value) {
                    return Ok(start);
                }
                pos = vwe;
                continue;
            }
            if base == "env" && is_env_assignment(&oword) {
                pos = owe;
                continue;
            }
            if base == "timeout" && duration_word(&oword) {
                pos = owe;
                continue;
            }
            break;
        }
    }
}

/// Blank every leading wrapper chain so the real verb sits at the `_CMDPOS` anchor.
pub fn strip_wrapper_prefixes(command: &str) -> Result<String, BudgetExceeded> {
    let text = Text::new(command);
    let mut cuts = Vec::new();
    for start in iter_shell_command_starts(&text)? {
        let end = wrapper_prefix_end(&text, start)?;
        if end > start {
            cuts.push((start, end));
        }
    }
    cuts.sort_unstable();
    let mut edits: Vec<(usize, usize, &str)> = Vec::new();
    let mut last_end: i64 = -1;
    for (start, end) in cuts {
        if start as i64 >= last_end {
            edits.push((start, end, " "));
            last_end = end as i64;
        }
    }
    Ok(splice(&text.chars, &edits))
}

fn continues(line: &str) -> bool {
    line.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

/// Maps a byte offset of `s` to a char offset (one entry per byte plus the end).
fn byte_to_char(s: &str) -> Vec<usize> {
    let mut map = vec![0; s.len() + 1];
    let mut count = 0;
    for (byte, c) in s.char_indices() {
        for slot in &mut map[byte..byte + c.len_utf8()] {
            *slot = count;
        }
        count += 1;
    }
    map[s.len()] = count;
    map
}

fn char_to_byte(s: &str, char_index: usize) -> usize {
    s.char_indices().nth(char_index).map_or(s.len(), |(b, _)| b)
}

/// Spans (char offsets) of quoted-delimiter heredoc bodies that a literal reader (`cat`, `tee`) only copies.
pub fn literal_heredoc_bodies(command: &str) -> Vec<(usize, usize)> {
    static HEREDOC: LazyLock<FancyRegex> = LazyLock::new(|| {
        FancyRegex::new(r#"<<-?[ \t]*(['"])([A-Za-z0-9_.-]+)\1([^\n]*)\n"#).expect("valid pattern")
    });
    static SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[|;&]").expect("valid pattern"));
    let chars: Vec<char> = command.chars().collect();
    let map = byte_to_char(command);
    let len = chars.len();
    let mut spans = Vec::new();
    let mut cursor = 0;
    for caps in HEREDOC.captures_iter(command).flatten() {
        let whole = caps.get(0).expect("group 0");
        let (m_start, m_end) = (map[whole.start()], map[whole.end()]);
        if m_start < cursor {
            continue;
        }
        let line_start = chars[..m_start]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |p| p + 1);
        let prefix: String = chars[line_start..m_start].iter().collect();
        let last_segment = prefix.rsplit([';', '&', '|', '(']).next().unwrap_or("");
        let Some(first) = last_segment.split(py_isspace).find(|w| !w.is_empty()) else {
            continue;
        };
        let reader = first.rsplit('/').next().unwrap_or("").to_lowercase();
        if reader != "cat" && reader != "tee" {
            continue;
        }
        let delimiter = caps.get(2).expect("group 2").as_str();
        let mut header = caps.get(3).expect("group 3").as_str().to_owned();
        let mut body_start = m_end;
        for _ in 0..9 {
            if !continues(&header) || SEP.is_match(&header) {
                break;
            }
            let line_end = chars[body_start.min(len)..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(len, |p| body_start + p);
            header.pop();
            header.extend(&chars[body_start.min(line_end)..line_end]);
            body_start = line_end + 1;
        }
        if SEP.is_match(&header) || continues(&header) || body_start > len {
            continue;
        }
        let close = FancyRegex::new(&format!(r"(?m)^\t*{}$", fancy_regex::escape(delimiter)))
            .ok()
            .and_then(|re| {
                re.find_from_pos(command, char_to_byte(command, body_start))
                    .ok()
                    .flatten()
            });
        match close {
            Some(m) => {
                cursor = map[m.end()];
                spans.push((body_start, map[m.start()]));
            }
            None => {
                cursor = len;
                spans.push((body_start, cursor));
            }
        }
    }
    spans
}

/// `command` with literal heredoc bodies blanked (newlines kept, so offsets and lines survive).
pub fn blank_literal_heredoc_bodies(command: &str) -> String {
    let spans = literal_heredoc_bodies(command);
    let mut chars: Vec<char> = command.chars().collect();
    for (start, end) in spans {
        for c in &mut chars[start..end.min(command.chars().count())] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    }
    chars.into_iter().collect()
}

const MAX_RM_OPERANDS: usize = 64;

/// `variant` with each quoted operand word of an `rm` command quote-removed, as the shell passes it.
pub fn dequote_rm_operands(variant: &str) -> Result<String, BudgetExceeded> {
    static RM: LazyLock<FancyRegex> = LazyLock::new(|| {
        FancyRegex::new(&format!("(?is){}", *RM_FLAG_PREFIX)).expect("valid pattern")
    });
    let text = Text::new(variant);
    let map = byte_to_char(variant);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut cursor = 0;
    for caps in RM.captures_iter(variant).flatten() {
        let mut pos = map[caps.get(0).expect("group 0").end()];
        if pos < cursor {
            continue;
        }
        for _ in 0..MAX_RM_OPERANDS {
            let (word_start, word_end, word) = read_shell_word(&text, pos)?;
            if word_start == word_end {
                break;
            }
            if word.contains(['\'', '"']) {
                let word_text = Text::new(&word);
                let mut quoted_glob = false;
                for step in scan_shell(&word_text, 0, None, ScanOpts::default()) {
                    let step = step?;
                    if step.kind == Kind::Char
                        && step.quote.is_some()
                        && "*?[".contains(word_text.chars[step.i])
                    {
                        quoted_glob = true;
                    }
                }
                if !quoted_glob {
                    let plain = strip_shell_word_syntax(&word)?;
                    if !plain.is_empty()
                        && !plain
                            .chars()
                            .any(|c| py_isspace(c) || ";|&()<>`".contains(c))
                    {
                        edits.push((word_start, word_end, plain));
                    }
                }
            }
            pos = word_end;
        }
        cursor = pos;
    }
    if edits.is_empty() {
        return Ok(variant.to_owned());
    }
    let borrowed: Vec<(usize, usize, &str)> =
        edits.iter().map(|(s, e, t)| (*s, *e, t.as_str())).collect();
    Ok(splice(&text.chars, &borrowed))
}
