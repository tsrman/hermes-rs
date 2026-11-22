//! Shell words and the literal-substitution deobfuscator (`_read_shell_word`,
//! `_deobfuscate_shell_word*`, `_literal_command_substitution_output`, a `shlex.split` clone).

use std::sync::LazyLock;

use regex::Regex;

use super::budget::{charge, memo_string, BudgetExceeded, Text};
use super::py_isspace;
use super::shell_scan::{scan_backtick_end, scan_dollar_paren_end, scan_shell, Kind, ScanOpts};

const MEMO_DEOBFUSCATE: u8 = 1;

pub fn skip_shell_whitespace(chars: &[char], mut pos: usize) -> usize {
    while pos < chars.len() && py_isspace(chars[pos]) {
        pos += 1;
    }
    pos
}

/// Read one shell word without executing expansions: `(start, end, word)`.
pub fn read_shell_word(text: &Text, pos: usize) -> Result<(usize, usize, String), BudgetExceeded> {
    let start = skip_shell_whitespace(&text.chars, pos);
    let mut end = start;
    let opts = ScanOpts {
        subst_unquoted: true,
        brace: true,
        ..ScanOpts::default()
    };
    for step in scan_shell(text, start, None, opts) {
        let step = step?;
        if step.kind == Kind::Char && step.quote.is_none() {
            let c = text.chars[step.i];
            if py_isspace(c) || ";&|<>()".contains(c) {
                break;
            }
        }
        end = step.j.unwrap_or(end);
    }
    Ok((start, end, text.chars[start..end].iter().collect()))
}

/// `shlex.split(s, posix=True)` (no comments); `None` where Python raises `ValueError`.
pub fn shlex_split(input: &str) -> Option<Vec<String>> {
    enum State {
        Space,
        Word,
        Quote(char),
        Escape,
    }
    let is_ws = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    let mut escaped_state = State::Space;
    for c in input.chars() {
        match state {
            State::Space => {
                if is_ws(c) {
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                } else if c == '\\' {
                    escaped_state = State::Word;
                    state = State::Escape;
                } else if c == '\'' || c == '"' {
                    state = State::Quote(c);
                } else {
                    token.push(c);
                    state = State::Word;
                }
            }
            State::Word => {
                if is_ws(c) {
                    state = State::Space;
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                } else if c == '\'' || c == '"' {
                    state = State::Quote(c);
                } else if c == '\\' {
                    escaped_state = State::Word;
                    state = State::Escape;
                } else {
                    token.push(c);
                }
            }
            State::Quote(q) => {
                quoted = true;
                if c == q {
                    state = State::Word;
                } else if c == '\\' && q == '"' {
                    escaped_state = State::Quote(q);
                    state = State::Escape;
                } else {
                    token.push(c);
                }
            }
            State::Escape => {
                if let State::Quote(q) = escaped_state {
                    if c != '\\' && c != q {
                        token.push('\\');
                    }
                }
                token.push(c);
                state = std::mem::replace(&mut escaped_state, State::Space);
            }
        }
    }
    match state {
        State::Quote(_) | State::Escape => None,
        State::Space | State::Word => {
            if !token.is_empty() || quoted {
                tokens.push(token);
            }
            Some(tokens)
        }
    }
}

fn simple_shell_literal(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c))
}

/// Resolve tiny literal command substitutions (`echo x`, `printf %s x`) without a shell.
pub fn literal_command_substitution_output(script: &str) -> Result<Option<String>, BudgetExceeded> {
    charge(script.chars().count() as i64)?;
    let Some(tokens) = shlex_split(script).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let command = tokens[0].to_lowercase();
    let mut args: &[String] = &tokens[1..];
    if command == "echo" {
        while let Some(first) = args.first() {
            let body = first.strip_prefix('-').unwrap_or("");
            if !body.is_empty() && body.chars().all(|c| matches!(c, 'n' | 'E' | 'e')) {
                args = &args[1..];
            } else {
                break;
            }
        }
    } else if command != "printf" {
        return Ok(None);
    }
    if args.len() == 1 && simple_shell_literal(&args[0]) {
        return Ok(Some(args[0].clone()));
    }
    if command == "printf" && args.len() == 2 && args[0] == "%s" && simple_shell_literal(&args[1]) {
        return Ok(Some(args[1].clone()));
    }
    Ok(None)
}

fn replace_simple_command_substitutions(word: &str) -> Result<String, BudgetExceeded> {
    let text = Text::new(word);
    let chars = &text.chars;
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let opener = if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
            2
        } else if chars[i] == '`' {
            1
        } else {
            0
        };
        let end = match opener {
            2 => scan_dollar_paren_end(&text, i)?,
            1 => scan_backtick_end(&text, i)?,
            _ => None,
        };
        let replacement = match end {
            Some(end) => {
                let script: String = chars[i + opener..end - 1].iter().collect();
                literal_command_substitution_output(&script)?
            }
            None => None,
        };
        match (replacement, end) {
            (Some(value), Some(end)) => {
                out.push_str(&value);
                i = end;
            }
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn replace_simple_shell_expansions(word: &str) -> Result<String, BudgetExceeded> {
    static REPLACEMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$\{[^}/\s\x1c-\x1f]+/[^}/]*/([^}]*)\}").expect("valid pattern")
    });
    static DEFAULT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\$\{[^}:\s\x1c-\x1f]+:-([^}]*)\}").expect("valid pattern"));
    let word = replace_simple_command_substitutions(word)?;
    let word = REPLACEMENT.replace_all(&word, "$1").into_owned();
    Ok(DEFAULT.replace_all(&word, "$1").into_owned())
}

pub fn strip_shell_word_syntax(word: &str) -> Result<String, BudgetExceeded> {
    let text = Text::new(word);
    let mut out = String::new();
    for step in scan_shell(&text, 0, None, ScanOpts::default()) {
        let step = step?;
        match step.kind {
            Kind::Quote => {}
            Kind::Esc => out.push(text.chars[step.i + 1]),
            _ => out.push(text.chars[step.i]),
        }
    }
    Ok(out)
}

fn deobfuscate_shell_word(word: &str) -> Result<String, BudgetExceeded> {
    static BRACE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\A\{([A-Za-z0-9_./:%@+=^-]+(?:,[A-Za-z0-9_./:%@+=^-]+)+)\}\z")
            .expect("valid pattern")
    });
    if let Some(caps) = BRACE.captures(word) {
        return Ok(caps[1].replace(',', " "));
    }
    let mut word = word.to_owned();
    for _ in 0..2 {
        let previous = word.clone();
        word = strip_shell_word_syntax(&replace_simple_shell_expansions(&word)?)?;
        if word == previous {
            break;
        }
    }
    Ok(word)
}

/// How shell syntax can spell a command word: quoting, escaping and simple literal command
/// substitutions collapsed. Narrow and non-executing; memoised per detection call.
pub fn deobfuscate_shell_word_for_detection(word: &str) -> Result<String, BudgetExceeded> {
    memo_string(MEMO_DEOBFUSCATE, word, || deobfuscate_shell_word(word))
}
