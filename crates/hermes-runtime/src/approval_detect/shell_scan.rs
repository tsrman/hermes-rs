//! The reference's quote/escape state machine and substitution finders
//! (`approval_parser_budget.py`: `_scan_shell`, `_scan_dollar_paren_end`, `_scan_backtick_end`,
//! `substitution_depth`). Offsets are `char` indices into a `Text`, as in Python.

use std::collections::VecDeque;

use super::ansi_c::is_shell_comment_start;
use super::budget::{charge, memo, BudgetExceeded, Text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Char,
    Esc,
    Quote,
    Subst,
    Comment,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Char => "char",
            Kind::Esc => "esc",
            Kind::Quote => "quote",
            Kind::Subst => "subst",
            Kind::Comment => "comment",
        }
    }
}

/// One lexical step: `text[i..j]` read in quote state `quote`. `j` is `None` only for an
/// unterminated substitution under `stop_unterminated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub kind: Kind,
    pub i: usize,
    pub j: Option<usize>,
    pub quote: Option<char>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOpts {
    /// Recognise substitutions outside quotes (`"u"` in the reference).
    pub subst_unquoted: bool,
    /// Recognise substitutions inside double quotes (`"q"`).
    pub subst_quoted: bool,
    pub brace: bool,
    pub stop_unterminated: bool,
    pub naive_backtick: bool,
    pub comments: bool,
}

pub struct Scan<'a> {
    text: &'a Text,
    n: usize,
    i: usize,
    quote: Option<char>,
    opts: ScanOpts,
    done: bool,
}

/// `_scan_shell(text, start, end, **opts)`; `end` of `None` is the whole text.
pub fn scan_shell(text: &Text, start: usize, end: Option<usize>, opts: ScanOpts) -> Scan<'_> {
    Scan {
        text,
        n: end.unwrap_or(text.len()),
        i: start,
        quote: None,
        opts,
        done: false,
    }
}

fn starts_with(chars: &[char], at: usize, pattern: &str) -> bool {
    (at..)
        .zip(pattern.chars())
        .all(|(k, p)| chars.get(k) == Some(&p))
}

/// Python's `str.find(ch, from, limit)`.
fn find_char(chars: &[char], ch: char, from: usize, limit: usize) -> Option<usize> {
    let limit = limit.min(chars.len());
    (from..limit).find(|&k| chars[k] == ch)
}

impl Scan<'_> {
    fn fail(&mut self, error: BudgetExceeded) -> Option<Result<Step, BudgetExceeded>> {
        self.done = true;
        Some(Err(error))
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<Step, BudgetExceeded>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.i >= self.n {
            return None;
        }
        if let Err(e) = charge(1) {
            return self.fail(e);
        }
        let text = self.text;
        let chars = &text.chars;
        let (i, quote) = (self.i, self.quote);
        let ch = chars[i];
        let mut kind = Kind::Char;
        let mut j = i + 1;
        if self.opts.comments && quote.is_none() && is_shell_comment_start(chars, i) {
            kind = Kind::Comment;
            j = find_char(chars, '\n', i, self.n).unwrap_or(self.n);
        } else if quote != Some('\'') && ch == '\\' && i + 1 < self.n {
            kind = Kind::Esc;
            j = i + 2;
        } else if Some(ch) == quote || (quote.is_none() && (ch == '\'' || ch == '"')) {
            kind = Kind::Quote;
        } else {
            let want = if quote.is_some() {
                self.opts.subst_quoted
            } else {
                self.opts.subst_unquoted
            };
            if quote != Some('\'')
                && want
                && (ch == '`'
                    || starts_with(chars, i, "$(")
                    || (self.opts.brace && quote.is_none() && starts_with(chars, i, "${")))
            {
                let close = if ch == '`' {
                    if self.opts.naive_backtick {
                        find_char(chars, '`', i + 1, chars.len()).map(|k| k + 1)
                    } else {
                        match scan_backtick_end(text, i) {
                            Ok(v) => v,
                            Err(e) => return self.fail(e),
                        }
                    }
                } else if chars[i + 1] == '(' {
                    match scan_dollar_paren_end(text, i) {
                        Ok(v) => v,
                        Err(e) => return self.fail(e),
                    }
                } else {
                    find_char(chars, '}', i + 2, chars.len()).map(|k| k + 1)
                };
                match close {
                    Some(close) => {
                        kind = Kind::Subst;
                        j = close;
                    }
                    None if self.opts.stop_unterminated => {
                        self.done = true;
                        return Some(Ok(Step {
                            kind: Kind::Subst,
                            i,
                            j: None,
                            quote,
                        }));
                    }
                    None => {}
                }
            }
        }
        if kind == Kind::Quote {
            self.quote = if quote.is_some() { None } else { Some(ch) };
        }
        self.i = j;
        Some(Ok(Step {
            kind,
            i,
            j: Some(j),
            quote,
        }))
    }
}

const MEMO_DOLLAR_PAREN: u8 = 1;
const MEMO_BACKTICK: u8 = 2;

/// Offset after a balanced `$(...)` starting at `start`.
pub fn scan_dollar_paren_end(text: &Text, start: usize) -> Result<Option<usize>, BudgetExceeded> {
    memo(MEMO_DOLLAR_PAREN, text, start, || {
        let mut depth: i64 = 1;
        for step in scan_shell(text, start + 2, None, ScanOpts::default()) {
            let step = step?;
            if step.kind == Kind::Char && step.quote.is_none() {
                depth += i64::from(starts_with(&text.chars, step.i, "$("))
                    - i64::from(text.chars[step.i] == ')');
                if depth == 0 {
                    return Ok(Some(step.i + 1));
                }
            }
        }
        Ok(None)
    })
}

/// Offset after the backtick span starting at `start`; only a backslash escapes in backticks.
pub fn scan_backtick_end(text: &Text, start: usize) -> Result<Option<usize>, BudgetExceeded> {
    memo(MEMO_BACKTICK, text, start, || {
        let chars = &text.chars;
        let mut k = start + 1;
        let mut end = None;
        while k < chars.len() {
            match chars[k] {
                '\\' if k + 1 < chars.len() => k += 2,
                '\\' => break,
                '`' => {
                    end = Some(k + 1);
                    break;
                }
                _ => k += 1,
            }
        }
        charge(end.unwrap_or(chars.len()) as i64 - start as i64)?;
        Ok(end)
    })
}

struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
    quoted: bool,
}

/// `<<(-?)[ \t]*((?:'[^'\n]*'|"[^"\n]*"|\\.|[^\s;&|<>()'"\\])+)` at `at`: (strip tabs, word, end).
fn match_heredoc(chars: &[char], at: usize) -> Option<(bool, String, usize)> {
    if !starts_with(chars, at, "<<") {
        return None;
    }
    let mut k = at + 2;
    let strip = chars.get(k) == Some(&'-');
    if strip {
        k += 1;
    }
    while matches!(chars.get(k), Some(' ' | '\t')) {
        k += 1;
    }
    let word_start = k;
    loop {
        match chars.get(k) {
            Some(&q @ ('\'' | '"')) => {
                let mut m = k + 1;
                while m < chars.len() && chars[m] != q && chars[m] != '\n' {
                    m += 1;
                }
                if chars.get(m) == Some(&q) {
                    k = m + 1;
                } else {
                    break;
                }
            }
            Some('\\') => {
                if matches!(chars.get(k + 1), Some(c) if *c != '\n') {
                    k += 2;
                } else {
                    break;
                }
            }
            Some(&c) if !(super::py_isspace(c) || ";&|<>()'\"\\".contains(c)) => k += 1,
            _ => break,
        }
    }
    if k == word_start {
        return None;
    }
    Some((strip, chars[word_start..k].iter().collect(), k))
}

fn quoted_heredoc_body_spans(
    chars: &[char],
    mut newline: usize,
    pending: &[Heredoc],
) -> Vec<(usize, usize)> {
    let len = chars.len();
    let mut spans = Vec::new();
    for heredoc in pending {
        let body_start = newline + 1;
        let mut line = String::new();
        while newline < len {
            let end = find_char(chars, '\n', newline + 1, len).unwrap_or(len);
            if newline + 1 < end {
                line.extend(&chars[newline + 1..end]);
            }
            newline = end;
            let trailing = line.chars().rev().take_while(|c| *c == '\\').count();
            if !heredoc.quoted && end < len && trailing % 2 == 1 {
                line.pop();
                continue;
            }
            let compare = if heredoc.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if compare == heredoc.delimiter {
                break;
            }
            line.clear();
        }
        if heredoc.quoted {
            spans.push((body_start, newline));
        }
        if newline == len {
            break;
        }
    }
    spans
}

struct Frame {
    quote: Option<char>,
    parens: usize,
    arith: bool,
}

/// Deepest `$(` nesting under shell quoting of the raw command (`substitution_depth`).
pub fn substitution_depth(command: &str) -> usize {
    depth_of(&command.chars().collect::<Vec<_>>())
}

fn depth_of(chars: &[char]) -> usize {
    let n = chars.len();
    let mut frames = vec![Frame {
        quote: None,
        parens: 0,
        arith: false,
    }];
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut skip_spans: VecDeque<(usize, usize)> = VecDeque::new();
    let (mut deepest, mut i) = (0usize, 0usize);
    while i < n {
        if let Some(&(start, end)) = skip_spans.front() {
            if i == start {
                skip_spans.pop_front();
                i = end;
                continue;
            }
        }
        let ch = chars[i];
        let top = frames.len() - 1;
        if ch == '#'
            && frames[top].quote.is_none()
            && (i == 0 || chars[i - 1] != ')')
            && is_shell_comment_start(chars, i)
        {
            match find_char(chars, '\n', i, n) {
                Some(k) => {
                    i = k;
                    continue;
                }
                None => break,
            }
        }
        if starts_with(chars, i, "<<<") {
            i += 3;
            continue;
        }
        if ch == '<' && frames[top].quote.is_none() && !frames[top].arith {
            if let Some((strip, word, end)) = match_heredoc(chars, i) {
                let quoted = word.chars().any(|c| matches!(c, '\'' | '"' | '\\'));
                let mut delimiter = String::new();
                let mut it = word.chars();
                while let Some(c) = it.next() {
                    if c == '\\' {
                        if let Some(next) = it.next() {
                            delimiter.push(next);
                            continue;
                        }
                    }
                    delimiter.push(c);
                }
                delimiter.retain(|c| c != '\'' && c != '"');
                heredocs.push(Heredoc {
                    delimiter,
                    strip_tabs: strip,
                    quoted,
                });
                i = end;
                continue;
            }
        }
        if ch == '\n' && frames[top].quote.is_none() && !heredocs.is_empty() {
            skip_spans.extend(quoted_heredoc_body_spans(chars, i, &heredocs));
            heredocs.clear();
        }
        if ch == '\'' && frames[top].quote.is_none() {
            let end = find_char(chars, '\'', i + 1, n).unwrap_or(n);
            let inner: &[char] = if i + 1 < end { &chars[i + 1..end] } else { &[] };
            deepest = deepest.max(frames.len() - 1 + depth_of(inner));
            i = end;
        } else if ch == '\\' {
            i += 1;
        } else if ch == '$' && chars.get(i + 1) == Some(&'(') {
            frames.push(Frame {
                quote: None,
                parens: 0,
                arith: starts_with(chars, i + 1, "(("),
            });
            deepest = deepest.max(frames.len() - 1);
            i += 1;
        } else if frames[top].quote == Some('"') {
            frames[top].quote = if ch == '"' { None } else { Some('"') };
        } else if ch == '"' {
            frames[top].quote = Some('"');
        } else if ch == '(' {
            frames[top].parens += 1;
        } else if ch == ')' {
            if frames[top].parens > 0 {
                frames[top].parens -= 1;
            } else if frames.len() > 1 {
                frames.pop();
            }
        }
        i += 1;
    }
    deepest
}
