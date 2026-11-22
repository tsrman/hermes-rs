//! Fuzzy find-and-replace for model-written edits, as `tools/fuzzy_match.py` does it.
//!
//! An ordered chain of increasingly permissive strategies lets whitespace, indentation,
//! escaping and Unicode drift in tool-call arguments still land on the intended region.
//! Positions are character indices, as in the reference.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::difflib::{ratio, SequenceMatcher};

type Span = (usize, usize);

pub(crate) const IDENTICAL_STRINGS_ERROR: &str = "No edit was applied because old_string and new_string are identical. Provide the existing text to replace in old_string and the changed replacement text in new_string.";
const GUTTER_STRIPPED_NOTE: &str = "old_string carried read_file's line-number prefixes (`N|`); they were stripped before matching. Send file text without the `N|` gutter next time.";
const GUTTER_NEW_STRING_ERROR: &str = "old_string looks like read_file output with its `N|` line-number prefixes, but new_string's `N|` prefixes are not a clean read_file gutter (missing on some non-blank lines, or not numbered consecutively), so the intended replacement text is ambiguous. Resend both strings without any `N|` prefixes.";
const GUTTER_MISS_SUFFIX: &str = " (its read_file `N|` line-number prefixes were stripped first; only an exact or whitespace-tolerant match of the remaining text is accepted)";

/// What a replacement did: the new text, how many places, the strategy that matched, and the
/// error when nothing was replaced.
pub(crate) struct Replacement {
    pub content: String,
    pub count: usize,
    pub strategy: Option<&'static str>,
    pub error: Option<String>,
}

fn unicode_replacement(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{201c}' | '\u{201d}' => "\"",
        '\u{2018}' | '\u{2019}' => "'",
        '\u{2014}' => "--",
        '\u{2013}' => "-",
        '\u{2026}' => "...",
        '\u{00a0}' => " ",
        '\u{2212}' => "-",
        '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => " ",
        _ => return None,
    })
}

fn unicode_normalize(text: &str) -> String {
    text.chars()
        .map(|c| unicode_replacement(c).map_or_else(|| c.to_string(), str::to_owned))
        .collect()
}

fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

fn lstrip(text: &str) -> &str {
    text.trim_start_matches(is_space)
}

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn slice(text: &[char], start: usize, end: usize) -> String {
    text[start.min(text.len())..end.min(text.len())]
        .iter()
        .collect()
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

// ── Position helpers ─────────────────────────────────────────────────────────────────────────────

fn line_positions(lines: &[&str], start: usize, end: usize, content_length: usize) -> Span {
    let start_pos: usize = lines[..start].iter().map(|l| char_len(l) + 1).sum();
    let end_pos = lines[..end].iter().map(|l| char_len(l) + 1).sum::<usize>() as isize - 1;
    (start_pos, content_length.min(end_pos.max(0) as usize))
}

fn window_spans(
    content_length: usize,
    lines: &[&str],
    n: usize,
    accept: impl Fn(usize) -> bool,
) -> Vec<Span> {
    if n > lines.len() {
        return Vec::new();
    }
    (0..=lines.len() - n)
        .filter(|i| accept(*i))
        .map(|i| line_positions(lines, i, i + n, content_length))
        .collect()
}

fn match_transformed_lines(
    content: &str,
    pattern: &str,
    transform: impl Fn(&[&str]) -> Vec<String>,
) -> Vec<Span> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let pattern_norm = transform(&pattern_lines);
    let n = pattern_norm.len();
    window_spans(char_len(content), &content_lines, n, |i| {
        transform(&content_lines[i..i + n]) == pattern_norm
    })
}

fn strip_boundary(lines: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    out[0] = strip(&out[0]).to_owned();
    if out.len() > 1 {
        let last = out.len() - 1;
        out[last] = strip(&out[last]).to_owned();
    }
    out
}

/// Map each original index to its index in the normalized text; the last entry is a sentinel.
fn orig_to_norm_map(original: &[char]) -> Vec<usize> {
    let mut out = Vec::with_capacity(original.len() + 1);
    let mut norm = 0;
    for c in original {
        out.push(norm);
        norm += unicode_replacement(*c).map_or(1, |r| r.chars().count());
    }
    out.push(norm);
    out
}

fn norm_end_to_orig(map: &[usize], orig_start: usize, norm_end: usize) -> usize {
    let length = map.len() - 1;
    let mut end = orig_start;
    while end < length && map[end] < norm_end {
        end += 1;
    }
    end
}

fn map_positions_norm_to_orig(map: &[usize], matches: &[Span]) -> Vec<Span> {
    let mut first: HashMap<usize, usize> = HashMap::new();
    for (orig, norm) in map[..map.len() - 1].iter().enumerate() {
        first.entry(*norm).or_insert(orig);
    }
    matches
        .iter()
        .filter_map(|(norm_start, norm_end)| {
            let start = *first.get(norm_start)?;
            Some((start, norm_end_to_orig(map, start, *norm_end)))
        })
        .collect()
}

/// Best-effort span mapping for `[ \t]+` -> `' '` whitespace collapsing.
fn map_normalized_positions(original: &[char], normalized: &[char], matches: &[Span]) -> Vec<Span> {
    let mut orig_to_norm: Vec<usize> = Vec::new();
    let (mut oi, mut ni) = (0, 0);
    while oi < original.len() && ni < normalized.len() {
        if original[oi] == normalized[ni] {
            orig_to_norm.push(ni);
            oi += 1;
            ni += 1;
        } else if matches!(original[oi], ' ' | '\t') && normalized[ni] == ' ' {
            orig_to_norm.push(ni);
            oi += 1;
            if oi < original.len() && !matches!(original[oi], ' ' | '\t') {
                ni += 1;
            }
        } else {
            orig_to_norm.push(ni);
            oi += 1;
        }
    }
    orig_to_norm.extend(std::iter::repeat_n(normalized.len(), original.len() - oi));
    let mut start_of: HashMap<usize, usize> = HashMap::new();
    let mut end_of: HashMap<usize, usize> = HashMap::new();
    for (orig, norm) in orig_to_norm.iter().enumerate() {
        start_of.entry(*norm).or_insert(orig);
        end_of.insert(*norm, orig);
    }
    let mut out = Vec::new();
    for (norm_start, norm_end) in matches {
        let orig_start = match start_of.get(norm_start) {
            Some(start) => *start,
            None => orig_to_norm
                .iter()
                .position(|n| n >= norm_start)
                .unwrap_or(0),
        };
        let mut orig_end = match end_of.get(&(norm_end - 1)) {
            Some(end) => end + 1,
            None => orig_start + (norm_end - norm_start),
        };
        // Absorb trailing collapsed whitespace only when the normalized match ended in a space.
        if *norm_end < normalized.len() && normalized[norm_end - 1] == ' ' {
            while orig_end < original.len() && matches!(original[orig_end], ' ' | '\t') {
                orig_end += 1;
            }
        }
        out.push((orig_start, orig_end.min(original.len())));
    }
    out
}

// ── Strategies ───────────────────────────────────────────────────────────────────────────────────

/// Exact, non-overlapping occurrences (`str.replace` semantics).
fn strategy_exact(content: &str, pattern: &str) -> Vec<Span> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut byte_to_char = 0;
    let mut last_byte = 0;
    for (byte, _) in content.match_indices(pattern) {
        byte_to_char += content[last_byte..byte].chars().count();
        let length = char_len(pattern);
        spans.push((byte_to_char, byte_to_char + length));
        byte_to_char += length;
        last_byte = byte + pattern.len();
    }
    spans
}

fn strategy_line_trimmed(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, |lines| {
        lines.iter().map(|l| strip(l).to_owned()).collect()
    })
}

fn collapse_blanks(text: &str) -> String {
    static RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").expect("valid regex"));
    RUN.replace_all(text, " ").into_owned()
}

fn strategy_whitespace_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let normalized = collapse_blanks(content);
    let found = strategy_exact(&normalized, &collapse_blanks(pattern));
    if found.is_empty() {
        return Vec::new();
    }
    map_normalized_positions(&chars(content), &chars(&normalized), &found)
}

fn strategy_indentation_flexible(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, |lines| {
        lines.iter().map(|l| lstrip(l).to_owned()).collect()
    })
}

fn strategy_escape_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let unescaped = pattern
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r");
    if unescaped == pattern {
        return Vec::new();
    }
    strategy_exact(content, &unescaped)
}

fn strategy_trimmed_boundary(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, strip_boundary)
}

fn strategy_unicode_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let norm_pattern = unicode_normalize(pattern);
    let norm_content = unicode_normalize(content);
    if norm_content == content && norm_pattern == pattern {
        return Vec::new();
    }
    let mut found = strategy_exact(&norm_content, &norm_pattern);
    if found.is_empty() {
        found = strategy_line_trimmed(&norm_content, &norm_pattern);
    }
    if found.is_empty() {
        return Vec::new();
    }
    map_positions_norm_to_orig(&orig_to_norm_map(&chars(content)), &found)
}

fn strategy_block_anchor(content: &str, pattern: &str) -> Vec<Span> {
    let pattern_norm = unicode_normalize(pattern);
    let pattern_lines: Vec<&str> = pattern_norm.split('\n').collect();
    if pattern_lines.len() < 2 {
        return Vec::new();
    }
    let first = strip(pattern_lines[0]);
    let last = strip(pattern_lines[pattern_lines.len() - 1]);
    let n = pattern_lines.len();
    let content_norm = unicode_normalize(content);
    let norm_lines: Vec<&str> = content_norm.split('\n').collect();
    if norm_lines.len() < n {
        return Vec::new();
    }
    let potential: Vec<usize> = (0..=norm_lines.len() - n)
        .filter(|i| strip(norm_lines[*i]) == first && strip(norm_lines[i + n - 1]) == last)
        .collect();
    // Looser thresholds matched unrelated blocks; these are the safe floor.
    let threshold = if potential.len() == 1 { 0.50 } else { 0.70 };
    let pattern_middle = pattern_lines[1..n - 1].join("\n");
    let original_lines: Vec<&str> = content.split('\n').collect();
    window_spans(char_len(content), &original_lines, n, |i| {
        if !potential.contains(&i) {
            return false;
        }
        if n <= 2 {
            return true;
        }
        let middle = norm_lines[i + 1..i + n - 1].join("\n");
        ratio(&middle, &pattern_middle) >= threshold
    })
}

fn strategy_context_aware(content: &str, pattern: &str) -> Vec<Span> {
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    let n = pattern_lines.len();
    if n > content_lines.len() {
        return Vec::new();
    }
    let first = strip(pattern_lines[0]);
    let last = strip(pattern_lines[n - 1]);
    let similarity = |a: &str, b: &str| if a == b { 1.0 } else { ratio(a, b) };
    window_spans(char_len(content), &content_lines, n, |i| {
        let block = &content_lines[i..i + n];
        if similarity(first, strip(block[0])) < 0.80 || similarity(last, strip(block[n - 1])) < 0.80
        {
            return false;
        }
        pattern_lines
            .iter()
            .zip(block)
            .all(|(p, c)| strip(p).is_empty() || similarity(strip(p), strip(c)) >= 0.80)
    })
}

type Strategy = (&'static str, fn(&str, &str) -> Vec<Span>);

const PRECISE: [Strategy; 7] = [
    ("exact", strategy_exact),
    ("line_trimmed", strategy_line_trimmed),
    ("whitespace_normalized", strategy_whitespace_normalized),
    ("indentation_flexible", strategy_indentation_flexible),
    ("escape_normalized", strategy_escape_normalized),
    ("trimmed_boundary", strategy_trimmed_boundary),
    ("unicode_normalized", strategy_unicode_normalized),
];
const SIMILARITY: [Strategy; 2] = [
    ("block_anchor", strategy_block_anchor),
    ("context_aware", strategy_context_aware),
];
const WHOLE_LINE: [Strategy; 1] = [("line_trimmed", strategy_line_trimmed)];

// ── Escape drift ─────────────────────────────────────────────────────────────────────────────────

fn backslash_runs(text: &str) -> Vec<usize> {
    let mut runs = Vec::new();
    let mut current = 0;
    for c in text.chars() {
        if c == '\\' {
            current += 1;
        } else if current > 0 {
            runs.push(current);
            current = 0;
        }
    }
    if current > 0 {
        runs.push(current);
    }
    runs
}

fn halve_backslash_runs(text: &str) -> String {
    let mut out = String::new();
    let mut run = 0;
    for c in text.chars() {
        if c == '\\' {
            run += 1;
            continue;
        }
        out.push_str(&"\\".repeat(run / 2));
        run = 0;
        out.push(c);
    }
    out.push_str(&"\\".repeat(run / 2));
    out
}

fn strip_quote_escapes(text: &str) -> String {
    text.replace("\\'", "'").replace("\\\"", "\"")
}

/// Every escaped quote is attributable to the wire artifact: a quote kind it touched is escaped
/// at every occurrence, by exactly one backslash.
fn quote_escapes_all_spurious(strings: &[&str]) -> bool {
    for quote in ['\'', '"'] {
        let escaped = format!("\\{quote}");
        if !strings.iter().any(|s| s.contains(&escaped)) {
            continue;
        }
        for text in strings {
            let chars: Vec<char> = text.chars().collect();
            for (index, c) in chars.iter().enumerate() {
                if *c != quote {
                    continue;
                }
                let mut run = 0;
                while run < index && chars[index - 1 - run] == '\\' {
                    run += 1;
                }
                if run != 1 {
                    return false;
                }
            }
        }
    }
    true
}

enum Drift {
    Corrected(String, String, String),
    Unchanged,
    Ambiguous(String),
}

fn maybe_correct_escape_drift(old: &str, new: &str, content: &str) -> Drift {
    // Quote escaping first.
    let suspect = (old.contains("\\'") && new.contains("\\'"))
        || (old.contains("\\\"") && new.contains("\\\""));
    if suspect && !content.contains(old) && quote_escapes_all_spurious(&[old, new]) {
        let stripped = strip_quote_escapes(old);
        if content.contains(&stripped) {
            let unproven: Vec<String> = ['\'', '"']
                .iter()
                .filter(|q| new.contains(&format!("\\{q}")) && !old.contains(&format!("\\{q}")))
                .map(|q| format!("\\{q}"))
                .collect();
            if !unproven.is_empty() {
                return Drift::Ambiguous(format!(
                    "Escape-drift detected: removing the spurious quote escapes in old_string matches the file, but new_string also contains {}, which old_string gives no evidence for. Re-read the file and resend old_string/new_string with their intended escaping.",
                    unproven.join(" and ")
                ));
            }
            return Drift::Corrected(
                stripped,
                strip_quote_escapes(new),
                "escape-drift auto-corrected: stripped a spurious backslash before a quote/apostrophe in old_string/new_string (a JSON-double-escaping wire artifact) before matching.".into(),
            );
        }
    }
    if old.contains('\\')
        && !content.contains(old)
        && backslash_runs(old).iter().all(|run| run % 2 == 0)
    {
        let halved = halve_backslash_runs(old);
        if content.contains(&halved) {
            if new.contains('\\') {
                return Drift::Ambiguous("Escape-drift detected: halving old_string's backslashes matches the file, but correcting new_string could remove intentional backslashes. Re-read the file and resend old_string/new_string with their intended escaping.".into());
            }
            return Drift::Corrected(
                halved,
                new.to_owned(),
                "escape-drift auto-corrected: halved doubled backslash runs in old_string before matching; new_string has no backslashes and was left unchanged.".into(),
            );
        }
    }
    Drift::Unchanged
}

fn matched_regions(content: &[char], matches: &[Span]) -> String {
    matches
        .iter()
        .map(|(s, e)| slice(content, *s, *e))
        .collect()
}

fn detect_escape_drift(content: &[char], matches: &[Span], old: &str, new: &str) -> Option<String> {
    let quote_suspects = new.contains("\\'") || new.contains("\\\"");
    if !quote_suspects && !old.contains('\\') {
        return None;
    }
    let regions = matched_regions(content, matches);
    if quote_suspects {
        for suspect in ["\\'", "\\\""] {
            if new.contains(suspect) && old.contains(suspect) && !regions.contains(suspect) {
                let plain = &suspect[1..];
                return Some(format!(
                    "Escape-drift detected: old_string and new_string contain the literal sequence {} but the matched region of the file does not. This is almost always a tool-call serialization artifact where an apostrophe or quote got prefixed with a spurious backslash. Re-read the file with read_file and pass old_string/new_string without backslash-escaping {} characters.",
                    py_repr_str(suspect),
                    py_repr_str(plain)
                ));
            }
        }
    }
    if new.contains("\\n") {
        let old_literals = old.matches("\\n").count();
        for (start, end) in matches {
            let region = slice(content, *start, *end);
            if region.contains('\n') && old_literals > region.matches("\\n").count() {
                return Some("Escape-drift detected: old_string contains more literal '\\\\n' sequences than the matched region of the file, which has real line breaks there instead. This is almost always a tool-call serialization artifact where a line break got escaped one extra time; new_string would write it as backslash + n. Re-read the file with read_file and pass old_string/new_string with actual line breaks, keeping only the '\\\\n' sequences that appear literally in the file.".into());
            }
        }
    }
    let old_runs = backslash_runs(old);
    let file_runs = backslash_runs(&regions);
    if old_runs.is_empty()
        || file_runs.is_empty()
        || old_runs.len() != file_runs.len()
        || old_runs == file_runs
        || old_runs.iter().zip(&file_runs).any(|(o, f)| *o != f * 2)
        || !(file_runs.iter().any(|f| *f >= 2) || file_runs.len() >= 2)
        || backslash_runs(new) == file_runs
    {
        return None;
    }
    Some("Escape-drift detected: every backslash run in old_string is exactly twice as long as in the matched region of the file (e.g. the file has `\\\\` where old_string has `\\\\\\\\`). The tool-call arguments were JSON-escaped one extra time; applying new_string verbatim would double every backslash in the file. Re-read the file with read_file and resend old_string/new_string with the backslash counts exactly as they appear in the file.".into())
}

/// Python's `repr` of a short string.
fn py_repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn maybe_unescape_new_string(new: &str, content: &[char], matches: &[Span]) -> String {
    if !new.contains("\\t") && !new.contains("\\r") {
        return new.to_owned();
    }
    let regions = matched_regions(content, matches);
    let mut out = new.to_owned();
    for (literal, control) in [("\\t", '\t'), ("\\r", '\r')] {
        if out.contains(literal) && regions.contains(control) {
            out = out.replace(literal, &control.to_string());
        }
    }
    out
}

// ── Replacement shaping ──────────────────────────────────────────────────────────────────────────

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn first_meaningful_line(text: &str) -> Option<&str> {
    text.split('\n').find(|line| !strip(line).is_empty())
}

fn match_starts_mid_line(content: &[char], start: usize, end: usize) -> bool {
    let region = slice(content, start, end);
    let mut first_line_start = start;
    for line in region.split('\n') {
        if !strip(line).is_empty() {
            let line_start = content[..first_line_start]
                .iter()
                .rposition(|c| *c == '\n')
                .map_or(0, |i| i + 1);
            let before = slice(content, line_start, first_line_start);
            return !before.trim_matches([' ', '\t', '\r']).is_empty();
        }
        first_line_start += char_len(line) + 1;
    }
    false
}

fn reindent_replacement(file_region: &str, old: &str, new: &str, mid_line: bool) -> String {
    if new.is_empty() || mid_line {
        return new.to_owned();
    }
    let (Some(old_first), Some(file_first)) = (
        first_meaningful_line(old),
        first_meaningful_line(file_region),
    ) else {
        return new.to_owned();
    };
    let old_indent = leading_whitespace(old_first);
    let file_indent = leading_whitespace(file_first);
    if old_indent == file_indent {
        return new.to_owned();
    }
    new.split('\n')
        .map(|line| {
            if strip(line).is_empty() {
                line.to_owned()
            } else if leading_whitespace(line).starts_with(old_indent) {
                format!("{file_indent}{}", &line[old_indent.len()..])
            } else {
                format!("{file_indent}{}", line.trim_start_matches([' ', '\t']))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply only the old->new edits onto the file region's original text, so a normalized match
/// does not flatten its typographic characters.
fn preserve_unicode(file_region: &str, new: &str, old: &str) -> String {
    let norm_old = unicode_normalize(old);
    if norm_old != unicode_normalize(file_region) {
        return new.to_owned();
    }
    let region = chars(file_region);
    let map = orig_to_norm_map(&region);
    let norm_old_chars = chars(&norm_old);
    let new_chars = chars(new);
    let mut out = String::new();
    for (tag, i1, i2, j1, j2) in SequenceMatcher::new(&norm_old_chars, &new_chars).opcodes() {
        if tag == "equal" {
            // The original character owning normalized index i1, even inside an expansion.
            let owner = map.partition_point(|value| *value <= i1) - 1;
            let end = norm_end_to_orig(&map, owner, i2);
            out.push_str(&slice(&region, owner, end));
        } else if tag != "delete" {
            out.push_str(&slice(&new_chars, j1, j2));
        }
    }
    out
}

fn apply_replacements(
    content: &[char],
    matches: &[Span],
    new: &str,
    old: Option<&str>,
    unicode: bool,
) -> String {
    let mut sorted: Vec<Span> = matches.to_vec();
    sorted.sort_by_key(|span| std::cmp::Reverse(span.0));
    let mut result: Vec<char> = content.to_vec();
    for (start, end) in sorted {
        let mut adjusted = new.to_owned();
        if let Some(old) = old {
            let region = slice(content, start, end);
            if unicode {
                adjusted = preserve_unicode(&region, &adjusted, old);
            }
            adjusted = reindent_replacement(
                &region,
                old,
                &adjusted,
                match_starts_mid_line(content, start, end),
            );
        }
        result.splice(start..end, adjusted.chars());
    }
    result.into_iter().collect()
}

// ── Orchestration ────────────────────────────────────────────────────────────────────────────────

fn format_match_locations(content: &[char], matches: &[Span]) -> String {
    const CAP: usize = 5;
    let mut rows: Vec<String> = Vec::new();
    for (start, _) in matches.iter().take(CAP) {
        let line_no = content[..*start].iter().filter(|c| **c == '\n').count() + 1;
        let line_start = content[..*start]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1);
        let line_end = content[line_start..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(content.len(), |i| line_start + i);
        let snippet = slice(content, line_start, line_end);
        let snippet = strip(&snippet).to_owned();
        let snippet = if char_len(&snippet) > 80 {
            format!("{}...", snippet.chars().take(77).collect::<String>())
        } else {
            snippet
        };
        rows.push(format!("  L{line_no}: {snippet}"));
    }
    if matches.len() > CAP {
        rows.push(format!("  ... and {} more", matches.len() - CAP));
    }
    rows.join("\n")
}

fn run_chain(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    strategies: &[Strategy],
) -> Option<Replacement> {
    let content_chars = chars(content);
    for (name, strategy) in strategies {
        let matches = strategy(content, old);
        if matches.is_empty() {
            continue;
        }
        let fail = |error: String| {
            Some(Replacement {
                content: content.to_owned(),
                count: 0,
                strategy: None,
                error: Some(error),
            })
        };
        if matches.len() > 1 && !replace_all {
            return fail(format!(
                "Found {} matches for old_string. Provide more context to make it unique, or use replace_all=True. Matches:\n{}",
                matches.len(),
                format_match_locations(&content_chars, &matches)
            ));
        }
        if replace_all && matches.len() > 1 && SIMILARITY.iter().any(|(s, _)| s == name) {
            return fail(format!(
                "Found {} approximate matches via the '{name}' strategy; replace_all only applies to exact matches. Provide the precise text (whitespace included) so an exact/line-trimmed match can be made.",
                matches.len()
            ));
        }
        // Non-exact matches came through some normalization, so new_string may carry drift.
        if *name != "exact" {
            if let Some(error) = detect_escape_drift(&content_chars, &matches, old, new) {
                return fail(error);
            }
        }
        let effective = maybe_unescape_new_string(new, &content_chars, &matches);
        let replaced = apply_replacements(
            &content_chars,
            &matches,
            &effective,
            (*name != "exact").then_some(old),
            *name == "unicode_normalized",
        );
        return Some(Replacement {
            content: replaced,
            count: matches.len(),
            strategy: Some(name),
            error: None,
        });
    }
    None
}

static GUTTER_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\|").expect("valid regex"));

/// `text` minus a read_file `N|` gutter, or `None` when it is not a verbatim paste.
fn strip_line_number_gutter(text: &str, min_numbered: usize) -> Option<String> {
    let mut stripped: Vec<String> = Vec::new();
    let mut expected: Option<u64> = None;
    let mut numbered = 0;
    for line in text.split('\n') {
        let found = GUTTER_LINE.captures(line);
        if found.is_none() && !strip(line).is_empty() {
            return None;
        }
        let mut line = line;
        if let Some(caps) = &found {
            let number: u64 = caps[1].parse().ok()?;
            if expected.is_some_and(|e| e != number) {
                return None;
            }
            expected = Some(number);
            numbered += 1;
            line = &line[caps.get(0)?.end()..];
        }
        if let Some(e) = expected.as_mut() {
            *e += 1;
        }
        stripped.push(line.to_owned());
    }
    (numbered >= min_numbered).then(|| stripped.join("\n"))
}

fn has_gutter_line(text: &str) -> bool {
    text.split('\n').any(|line| GUTTER_LINE.is_match(line))
}

/// True when the edit is already present (a re-sent edit is a success-shaped no-op). Conservative:
/// the new text is non-trivial (8 or more characters) and present exactly, and the old text is gone.
pub(crate) fn is_already_applied(content: &str, old: &str, new: &str) -> bool {
    if new.is_empty() || char_len(strip(new)) < 8 || !content.contains(new) {
        return false;
    }
    old == new || !content.contains(old)
}

/// The note for an edit whose span was found by a similarity strategy: such a window only
/// resembles the search text, so the span can land a line off.
pub(crate) fn boundary_note(strategy: Option<&str>) -> Option<String> {
    let name = strategy.filter(|name| SIMILARITY.iter().any(|(s, _)| s == name))?;
    Some(format!(
        "Non-exact match (strategy: {name}). The edit span was located approximately, not matched to old_string's literal boundaries — review the diff below to confirm the change landed exactly where intended (a shifted boundary can duplicate or drop a line just outside the edited region)."
    ))
}

/// Find `old` in `content` through the strategy chain and replace it with `new`.
pub(crate) fn find_and_replace(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    notes: &mut Vec<String>,
) -> Replacement {
    let failure = |error: String| Replacement {
        content: content.to_owned(),
        count: 0,
        strategy: None,
        error: Some(error),
    };
    if old.is_empty() {
        return failure("old_string is empty — nothing to match. Set old_string to the exact existing text the replacement should replace (read the file first if unsure). To create a new file or fully rewrite one, use write_file instead. Do not re-send this call unchanged.".into());
    }
    if strip(old).is_empty() {
        return failure("old_string is only whitespace — provide non-blank text to match. Set it to the exact existing text the replacement should replace (read the file first if unsure). Do not re-send this call unchanged.".into());
    }
    let (old, new, correction) = match maybe_correct_escape_drift(old, new, content) {
        Drift::Ambiguous(message) => return failure(message),
        Drift::Corrected(old, new, note) => (old, new, Some(note)),
        Drift::Unchanged => (old.to_owned(), new.to_owned(), None),
    };
    if old == new {
        return failure(IDENTICAL_STRINGS_ERROR.into());
    }
    // A raw precise match wins (the file may really hold `N|` text); otherwise a read_file
    // gutter paste is stripped BEFORE any similarity strategy sees it.
    let mut result = run_chain(content, &old, &new, replace_all, &PRECISE);
    let stripped = if result.is_none() {
        strip_line_number_gutter(&old, 1)
            .filter(|text| !strip(text).is_empty() && !has_gutter_line(content))
            .map(|text| {
                let strategies: &[Strategy] = if strip_line_number_gutter(&old, 2).is_none() {
                    &WHOLE_LINE
                } else {
                    &PRECISE
                };
                (text, strategies)
            })
    } else {
        None
    };
    if result.is_none() {
        if let Some((stripped_old, strategies)) = &stripped {
            let stripped_new = match strip_line_number_gutter(&new, 1) {
                Some(text) => text,
                None if has_gutter_line(&new) => return failure(GUTTER_NEW_STRING_ERROR.into()),
                None => new.clone(),
            };
            if *stripped_old == stripped_new {
                return failure(IDENTICAL_STRINGS_ERROR.into());
            }
            result = run_chain(
                content,
                stripped_old,
                &stripped_new,
                replace_all,
                strategies,
            );
            if result.as_ref().is_some_and(|r| r.error.is_none()) {
                notes.push(GUTTER_STRIPPED_NOTE.into());
            }
        } else {
            result = run_chain(content, &old, &new, replace_all, &SIMILARITY);
        }
    }
    match result {
        None => failure(format!(
            "Could not find a match for old_string in the file{}",
            if stripped.is_some() {
                GUTTER_MISS_SUFFIX
            } else {
                ""
            }
        )),
        Some(found) => {
            if found.error.is_none() {
                if let Some(note) = correction {
                    notes.push(note);
                }
            }
            found
        }
    }
}

// ── "Did you mean?" ──────────────────────────────────────────────────────────────────────────────

fn visualize_whitespace(line: &str) -> String {
    let stripped = line.trim_start_matches([' ', '\t']);
    let prefix = &line[..line.len() - stripped.len()];
    format!("{}{stripped}", prefix.replace('\t', "→").replace(' ', "·"))
}

/// Python's `str.splitlines()`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((index, c)) = iter.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            lines.push(&text[start..index]);
            let mut next = index + c.len_utf8();
            if c == '\r' && iter.peek().is_some_and(|(_, after)| *after == '\n') {
                iter.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Numbered snippets of the lines most similar to `old`'s anchor line, or "".
pub(crate) fn find_closest_lines(old: &str, content: &str) -> String {
    const CONTEXT: usize = 2;
    if old.is_empty() || content.is_empty() {
        return String::new();
    }
    let old_lines = split_lines(old);
    let content_lines = split_lines(content);
    if old_lines.is_empty() || content_lines.is_empty() {
        return String::new();
    }
    let anchor = if strip(old_lines[0]).is_empty() {
        old_lines
            .iter()
            .map(|l| strip(l))
            .find(|l| !l.is_empty())
            .unwrap_or("")
    } else {
        strip(old_lines[0])
    };
    if anchor.is_empty() {
        return String::new();
    }
    let mut scored: Vec<(f64, usize)> = content_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !strip(line).is_empty())
        .map(|(index, line)| (ratio(anchor, strip(line)), index))
        .collect();
    // Python's sort is stable and keyed on the negated score.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let top: Vec<(f64, usize)> = scored.into_iter().filter(|s| s.0 > 0.3).take(3).collect();
    if top.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut seen: Vec<(usize, usize)> = Vec::new();
    for (_, index) in &top {
        let start = index.saturating_sub(CONTEXT);
        let end = content_lines.len().min(index + old_lines.len() + CONTEXT);
        if seen.contains(&(start, end)) {
            continue;
        }
        seen.push((start, end));
        parts.push(
            (0..end - start)
                .map(|j| format!("{:4}| {}", start + j + 1, content_lines[start + j]))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let mut result = parts.join("\n---\n");
    let best = content_lines[top[0].1];
    if strip(best) == anchor && best != old_lines[0] {
        result.push_str(&format!(
            "\n\nWhitespace difference detected (→ = tab, · = space):\n  file has: {}\n  you sent: {}\nUse the exact whitespace shown in 'file has'.",
            visualize_whitespace(best),
            visualize_whitespace(old_lines[0])
        ));
    }
    result
}

/// The "Did you mean" snippet appended to a plain no-match error, else "".
pub(crate) fn no_match_hint(error: &str, count: usize, old: &str, content: &str) -> String {
    if count != 0 || !error.starts_with("Could not find") {
        return String::new();
    }
    let hint = find_closest_lines(old, content);
    if hint.is_empty() {
        String::new()
    } else {
        format!("\n\nDid you mean one of these sections?\n{hint}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    #[test]
    fn replacements_match_the_reference() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/fuzzy_cases.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut notes = Vec::new();
            let found = find_and_replace(
                case["content"].as_str().unwrap(),
                case["old"].as_str().unwrap(),
                case["new"].as_str().unwrap(),
                case["replace_all"] == true,
                &mut notes,
            );
            let want = &case["result"];
            assert_eq!(json!(found.content), want[0], "{name}: content");
            assert_eq!(json!(found.count), want[1], "{name}: count");
            assert_eq!(json!(found.strategy), want[2], "{name}: strategy");
            assert_eq!(json!(found.error), want[3], "{name}: error");
            assert_eq!(json!(notes), case["notes"], "{name}: notes");
        }
    }

    #[test]
    fn closest_lines_match_the_reference() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/fuzzy_cases.json")).unwrap();
        for case in fixture["hints"].as_array().unwrap() {
            let (old, content) = (
                case["old"].as_str().unwrap(),
                case["content"].as_str().unwrap(),
            );
            assert_eq!(
                json!(find_closest_lines(old, content)),
                case["closest"],
                "{old:?}"
            );
            assert_eq!(
                json!(no_match_hint(
                    "Could not find a match for old_string in the file",
                    0,
                    old,
                    content
                )),
                case["hint"],
                "{old:?}"
            );
        }
    }

    #[test]
    fn diffs_and_notes_match_the_reference() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/fuzzy_cases.json")).unwrap();
        for case in fixture["diffs"].as_array().unwrap() {
            let (old, new) = (case["old"].as_str().unwrap(), case["new"].as_str().unwrap());
            assert_eq!(
                json!(crate::difflib::unified_diff(old, new, "a/f.txt", "b/f.txt")),
                case["diff"],
                "{:?} -> {:?}",
                old.chars().take(30).collect::<String>(),
                new.chars().take(30).collect::<String>()
            );
        }
        for case in fixture["applied"].as_array().unwrap() {
            let (c, o, n) = (
                case["content"].as_str().unwrap(),
                case["old"].as_str().unwrap(),
                case["new"].as_str().unwrap(),
            );
            assert_eq!(
                is_already_applied(c, o, n),
                case["applied"] == true,
                "{c:?} {o:?} {n:?}"
            );
        }
        for case in fixture["notes"].as_array().unwrap() {
            assert_eq!(
                json!(boundary_note(case["strategy"].as_str())),
                case["note"],
                "{}",
                case["strategy"]
            );
        }
    }
}
