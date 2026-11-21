//! Keys with a known vendor prefix (`sk-…`, `ghp_…`), including keys split by control
//! characters or terminal colour codes.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

use super::mask::mask_token;

/// Every pattern opens on a literal prefix. Copied from the reference.
const PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-](?:\.?[A-Za-z0-9_-]){9,}",
    r"ctx7sk-[A-Za-z0-9_-]{10,}",
    r"ghp_[A-Za-z0-9]{10,}",
    r"github_pat_[A-Za-z0-9_]{10,}",
    r"gho_[A-Za-z0-9]{10,}",
    r"ghu_[A-Za-z0-9]{10,}",
    r"ghs_[A-Za-z0-9]{10,}",
    r"ghr_[A-Za-z0-9]{10,}",
    r"xapp-\d+-[A-Za-z0-9-]{10,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AIza[A-Za-z0-9_-]{30,}",
    r"GOCSPX-[A-Za-z0-9_-]{8,}",
    r"ya29\.(?:[a-z]\.)?[A-Za-z0-9_-]{20,}",
    r"AQ\.[A-Za-z0-9_-]{40,}",
    r"1//0[A-Za-z0-9_-]{20,}",
    r"pplx-[A-Za-z0-9]{10,}",
    r"fal_[A-Za-z0-9_-]{10,}",
    r"fc-[A-Za-z0-9]{10,}",
    r"bb_live_[A-Za-z0-9_-]{10,}",
    r"gAAAA[A-Za-z0-9_=-]{20,}",
    r"AKIA[A-Z0-9]{16}",
    r"sk_live_[A-Za-z0-9]{10,}",
    r"sk_test_[A-Za-z0-9]{10,}",
    r"rk_live_[A-Za-z0-9]{10,}",
    r"SG\.[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_-]{10,})?",
    r"hf_[A-Za-z0-9]{10,}",
    r"r8_[A-Za-z0-9]{10,}",
    r"npm_[A-Za-z0-9]{10,}",
    r"pypi-[A-Za-z0-9_-]{10,}",
    r"dop_v1_[A-Za-z0-9]{10,}",
    r"doo_v1_[A-Za-z0-9]{10,}",
    r"am_(?:org_)?[A-Za-z0-9]{20,}",
    r"sk_[A-Za-z0-9_]{10,}",
    r"tvly-(?:(?:dev|prod)-)?[A-Za-z0-9]{10,}",
    r"exa_[A-Za-z0-9]{10,}",
    r"gsk_[A-Za-z0-9]{10,}",
    r"syt_[A-Za-z0-9]{10,}",
    r"retaindb_[A-Za-z0-9]{10,}",
    r"hsk-[A-Za-z0-9]{10,}",
    r"mem0_[A-Za-z0-9]{10,}",
    r"brv_[A-Za-z0-9]{10,}",
    r"xai-[A-Za-z0-9]{30,}",
    r"ntn_[A-Za-z0-9]{10,}",
    r"fw-[A-Za-z0-9]{30,}",
    r"fw_[A-Za-z0-9]{30,}",
    r"fpk_[A-Za-z0-9]{30,}",
    r"glpat-[A-Za-z0-9_\-]{10,}",
    r"gloas-[A-Za-z0-9_\-]{10,}",
    r"gldt-[A-Za-z0-9_\-]{10,}",
    r"glrt-[A-Za-z0-9_.\-]{10,}",
    r"glrtr-[A-Za-z0-9_.\-]{10,}",
    r"glcbt-[A-Za-z0-9_\-]{10,}",
    r"glptt-[A-Za-z0-9_\-]{10,}",
    r"glft-[A-Za-z0-9_\-]{10,}",
    r"glimt-[A-Za-z0-9_\-]{10,}",
    r"glagent-[A-Za-z0-9_\-]{10,}",
    r"glsoat-[A-Za-z0-9_\-]{10,}",
    r"glffct-[A-Za-z0-9_\-]{10,}",
    r"glwt-[A-Za-z0-9_\-]{10,}",
    r"GR1348941[A-Za-z0-9_\-]{10,}",
    r"glsa_[A-Za-z0-9]{10,}(?:_[A-Fa-f0-9]{8})?",
    r"glc_eyJ[A-Za-z0-9+/]{29,}={0,3}",
    r"pk-lf-[A-Za-z0-9\-]{8,}",
];

/// Leading literal characters of each pattern, up to its first metacharacter.
pub(super) fn literal_prefixes() -> &'static [String] {
    static PREFIXES: LazyLock<Vec<String>> = LazyLock::new(|| {
        PATTERNS
            .iter()
            .map(|pattern| {
                let end = pattern
                    .find(|c: char| "[(\\.?*+|{^$".contains(c))
                    .unwrap_or(pattern.len());
                pattern[..end].to_owned()
            })
            .collect()
    });
    &PREFIXES
}

/// Text of the last `[...]` class in `pattern`, escape-aware.
fn last_char_class(pattern: &str) -> Option<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut last = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '[' {
            let mut j = i + 1;
            if chars.get(j) == Some(&'^') {
                j += 1;
            }
            if chars.get(j) == Some(&']') {
                j += 1;
            }
            while j < chars.len() && chars[j] != ']' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            last = Some(chars[i..=j.min(chars.len() - 1)].iter().collect::<String>());
            i = j + 1;
            continue;
        }
        i += 1;
    }
    last
}

/// Lookahead ending a pattern: a following alphanumeric blocks the match, and `_` or `-` do
/// only when the body class admits them.
fn trailing_guard(pattern: &str) -> String {
    let admitted = match last_char_class(pattern) {
        None => "_-".to_owned(),
        Some(class) => {
            let whole = Regex::new(&format!("^{class}$"));
            match whole {
                Ok(whole) => ["_", "-"]
                    .iter()
                    .filter(|c| whole.is_match(c))
                    .copied()
                    .collect::<String>(),
                Err(_) => "_-".to_owned(),
            }
        }
    };
    format!("(?![A-Za-z0-9{admitted}])")
}

fn escape(c: char) -> String {
    regex::escape(&c.to_string())
}

/// The matcher for contiguous keys: not preceded by an identifier character.
static PREFIX_RE: LazyLock<Fancy> = LazyLock::new(|| {
    let branches: Vec<String> = PATTERNS
        .iter()
        .map(|p| format!("(?:{p}){}", trailing_guard(p)))
        .collect();
    Fancy::new(&format!("(?<![A-Za-z0-9_-])({})", branches.join("|")))
        .expect("valid prefix pattern")
});

/// The masking matcher also admits a key glued to an identifier by `-` or `_`; the caller
/// keeps such a match only when its body is opaque.
static PREFIX_SUB_RE: LazyLock<Fancy> = LazyLock::new(|| {
    let mut groups: Vec<(char, Vec<String>)> = Vec::new();
    for pattern in PATTERNS {
        let first = pattern.chars().next().expect("pattern is not empty");
        let rest = format!(
            "(?:{}){}",
            &pattern[first.len_utf8()..],
            trailing_guard(pattern)
        );
        match groups.iter_mut().find(|(c, _)| *c == first) {
            Some((_, rests)) => rests.push(rest),
            None => groups.push((first, vec![rest])),
        }
    }
    let body: Vec<String> = groups
        .iter()
        .map(|(c, rests)| format!("{}(?:{})", escape(*c), rests.join("|")))
        .collect();
    Fancy::new(&format!("(?<![A-Za-z0-9])({})", body.join("|"))).expect("valid prefix pattern")
});

/// A run of 16 or more letters and digits that is neither one case of letters nor digits only.
fn is_opaque_prefixed_token(token: &str) -> bool {
    let mut run = String::new();
    let opaque = |run: &str| -> bool {
        if run.chars().count() < 16 {
            return false;
        }
        let all_alpha = run.chars().all(|c| c.is_ascii_alphabetic());
        let lower = run.chars().all(|c| c.is_ascii_lowercase());
        let upper = run.chars().all(|c| c.is_ascii_uppercase());
        let digits = run.chars().all(|c| c.is_ascii_digit());
        !((all_alpha && (lower || upper)) || digits)
    };
    for c in token.chars().chain(std::iter::once('\0')) {
        if c.is_ascii_alphanumeric() {
            run.push(c);
        } else {
            if opaque(&run) {
                return true;
            }
            run.clear();
        }
    }
    false
}

/// A match glued to an identifier by `-` or `_` whose body is not opaque (`scikit-learn`).
fn is_glued_identifier(text: &[char], start: usize, token: &str) -> bool {
    start > 0
        && matches!(text[start - 1], '-' | '_')
        && !(token.starts_with("GOCSPX-")
            || token.starts_with("ya29.")
            || token.starts_with("1//0"))
        && !is_opaque_prefixed_token(token)
}

/// A text with the positions of its characters, because the patterns report byte offsets and
/// the rules below work on character positions.
struct Indexed<'a> {
    text: &'a str,
    /// `chars[i]` is the character at character position `i`.
    chars: Vec<char>,
    /// Byte offset of each character position, and the length at the end.
    offsets: Vec<usize>,
}

impl<'a> Indexed<'a> {
    fn new(text: &'a str) -> Self {
        let mut offsets: Vec<usize> = text.char_indices().map(|(offset, _)| offset).collect();
        offsets.push(text.len());
        Self {
            text,
            chars: text.chars().collect(),
            offsets,
        }
    }

    fn char_pos(&self, byte: usize) -> usize {
        self.offsets.partition_point(|&offset| offset < byte)
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.chars[start..end].iter().collect()
    }
}

/// `(start, end)` in characters of group 1 for one match.
type Span = (usize, usize);

fn group_span(indexed: &Indexed<'_>, captures: &fancy_regex::Captures<'_, str>) -> Option<Span> {
    let group = captures.get(1)?;
    Some((
        indexed.char_pos(group.start()),
        indexed.char_pos(group.end()),
    ))
}

/// Every match, plus each match that starts inside one and runs past its end: a key that a
/// longer earlier match swallowed would otherwise never be seen.
fn iter_with_swallowed(pattern: &Fancy, indexed: &Indexed<'_>) -> Vec<Span> {
    let mut found = Vec::new();
    let mut position = 0;
    while position <= indexed.text.len() {
        let Ok(Some(captures)) = pattern.captures_from_pos(indexed.text, position) else {
            break;
        };
        let whole = captures.get(0).expect("group 0");
        let Some(span) = group_span(indexed, &captures) else {
            break;
        };
        found.push(span);
        let group_start = captures.get(1).map_or(whole.start(), |g| g.start());
        let mut inner_from = group_start
            + indexed.text[group_start..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
        let group_end_char = span.1;
        while let Ok(Some(inner)) = pattern.captures_from_pos(indexed.text, inner_from) {
            let Some(inner_span) = group_span(indexed, &inner) else {
                break;
            };
            if inner_span.0 >= group_end_char {
                break;
            }
            if inner_span.1 > group_end_char {
                found.push(inner_span);
                break;
            }
            inner_from = indexed.offsets[inner_span.1.min(indexed.offsets.len() - 1)];
        }
        // Continue after this match like `finditer`: an empty match moves one character on.
        position = if whole.end() > whole.start() {
            whole.end()
        } else {
            whole.end()
                + indexed.text[whole.end()..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8)
        };
    }
    found
}

/// Merge overlapping spans; a merged flag is true only when every part is.
fn union(mut spans: Vec<(usize, usize, bool)>) -> Vec<(usize, usize, bool)> {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize, bool)> = Vec::new();
    for (start, end, flag) in spans {
        match merged.last_mut() {
            Some(last) if start < last.1 => {
                last.1 = last.1.max(end);
                last.2 = last.2 && flag;
            }
            _ => merged.push((start, end, flag)),
        }
    }
    merged
}

/// Mask every key that the contiguous matcher finds, treating overlaps as one span.
fn mask_prefix_matches(text: &str) -> String {
    let indexed = Indexed::new(text);
    let spans = iter_with_swallowed(&PREFIX_SUB_RE, &indexed)
        .into_iter()
        .filter(|&(start, end)| {
            !is_glued_identifier(&indexed.chars, start, &indexed.slice(start, end))
        })
        .map(|(start, end)| (start, end, true))
        .collect();
    let mut out = String::new();
    let mut position = 0;
    for (start, end, _) in union(spans) {
        out.push_str(&indexed.slice(position, start));
        out.push_str(&mask_token(&indexed.slice(start, end)));
        position = end;
    }
    out.push_str(&indexed.slice(position, indexed.chars.len()));
    out
}

static CONTROL_CHARS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\x00-\x1f\x7f\x{200b}-\x{200f}\x{2028}-\x{202f}\x{2060}\x{feff}]$")
        .expect("valid pattern")
});

/// A complete terminal escape sequence at the start of the slice.
static CSI_SEQ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:\x1b\[|\x{9b})[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x1b(?:[\x20-\x2f]+[\x30-\x7e]|[NO][^\x00-\x1f\x7f]|[78=>M]))",
    )
    .expect("valid pattern")
});

fn is_token_body_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/')
}

/// The text without escape sequences and control characters, which character of the text each
/// kept character came from, and which characters were removed.
fn strip_shadow(text: &str) -> (String, Vec<bool>, Vec<usize>) {
    let indexed = Indexed::new(text);
    let mut shadow = String::new();
    let mut noise = vec![false; indexed.chars.len()];
    let mut origin = Vec::new();
    let mut i = 0;
    while i < indexed.chars.len() {
        let c = indexed.chars[i];
        if matches!(c, '\x1b' | '\u{9b}') {
            if let Some(found) = CSI_SEQ.find(&text[indexed.offsets[i]..]) {
                let length = found.as_str().chars().count();
                noise[i..i + length].iter_mut().for_each(|n| *n = true);
                i += length;
                continue;
            }
        }
        if CONTROL_CHARS.is_match(&c.to_string()) {
            noise[i] = true;
        } else {
            shadow.push(c);
            origin.push(i);
        }
        i += 1;
    }
    (shadow, noise, origin)
}

/// Mask keys that control characters or colour codes split, matching on a copy without the noise.
fn mask_control_split_tokens(text: &str) -> String {
    let has_control = text
        .chars()
        .any(|c| CONTROL_CHARS.is_match(&c.to_string()) || c == '\u{9b}');
    if !has_control {
        return text.to_owned();
    }
    let original = Indexed::new(text);
    let (shadow_text, noise, origin) = strip_shadow(text);
    if shadow_text.chars().count() == original.chars.len() {
        return text.to_owned();
    }
    let shadow = Indexed::new(&shadow_text);
    let mut spans: Vec<(usize, usize, bool)> = Vec::new();
    for (pattern, skipping) in [(&*PREFIX_RE, false), (&*PREFIX_SUB_RE, true)] {
        for (body_start, body_end) in iter_with_swallowed(pattern, &shadow) {
            let start = origin[body_start];
            let end = origin[body_end - 1] + 1;
            let mut span = original.slice(start, end);
            if skipping {
                // The live text up to the first tab or line break, as the contiguous pass reads it.
                let first_line: String = span
                    .chars()
                    .take_while(|c| !matches!(c, '\t' | '\n' | '\r'))
                    .collect();
                let field: String = first_line
                    .chars()
                    .enumerate()
                    .filter(|(k, _)| !noise[start + k])
                    .map(|(_, c)| c)
                    .collect();
                let glued = shadow_start_glued(&shadow, body_start);
                if glued && is_glued_identifier_shadow(&shadow, body_start, &field) {
                    continue;
                }
            }
            let mut end = end;
            // A span across a line break whose first line is already a token must not join the next.
            if (span.contains('\n') || span.contains('\r'))
                && PREFIX_RE.is_match(&span).unwrap_or(false)
            {
                let cut = span
                    .chars()
                    .position(|c| matches!(c, '\n' | '\r'))
                    .unwrap_or(0);
                let line_end = origin.partition_point(|&o| o < start + cut);
                let clipped_end = origin[line_end - 1] + 1;
                let body = shadow.slice(body_start, line_end);
                end = clipped_end;
                span = original.slice(start, end);
                let glued_left = start > 0 && noise[start - 1];
                let touched = noise[start..end].iter().any(|n| *n);
                let full = PREFIX_RE
                    .find(&body)
                    .ok()
                    .flatten()
                    .is_some_and(|m| m.start() == 0 && m.end() == body.len());
                if !(glued_left || touched) || !full {
                    continue;
                }
            }
            let trimmed = span.trim_end_matches('=');
            let live_ok = trimmed
                .chars()
                .enumerate()
                .all(|(k, c)| noise[start + k] || is_token_body_char(c));
            if live_ok && (end >= original.chars.len() || original.chars[end] != '=') {
                spans.push((start, end, false));
            }
        }
    }
    if spans.is_empty() {
        return text.to_owned();
    }
    // A merged span may end inside a plain glued key that the contiguous pass would mask.
    for (start, end) in iter_with_swallowed(&PREFIX_SUB_RE, &original) {
        if !is_glued_identifier(&original.chars, start, &original.slice(start, end)) {
            spans.push((start, end, true));
        }
    }
    let mut out = String::new();
    let mut position = 0;
    for (start, end, plain) in union(spans) {
        if !plain {
            out.push_str(&original.slice(position, start));
            let live: String = (start..end)
                .filter(|&k| !noise[k])
                .map(|k| original.chars[k])
                .collect();
            out.push_str(&mask_token(&live));
            position = end;
        }
    }
    out.push_str(&original.slice(position, original.chars.len()));
    out
}

fn shadow_start_glued(shadow: &Indexed<'_>, start: usize) -> bool {
    start > 0 && matches!(shadow.chars[start - 1], '-' | '_')
}

fn is_glued_identifier_shadow(shadow: &Indexed<'_>, start: usize, token: &str) -> bool {
    is_glued_identifier(&shadow.chars, start, token)
}

/// Mask vendor-prefixed keys.
pub(super) fn redact(text: &str) -> String {
    let lowered_gate = PATTERNS.iter().any(|pattern| {
        let literal: String = pattern
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'))
            .collect();
        !literal.is_empty() && text.contains(&literal)
    });
    // Splits can hide a prefix from the gate above, so a text with control characters always runs.
    if !lowered_gate
        && !text
            .chars()
            .any(|c| CONTROL_CHARS.is_match(&c.to_string()) || c == '\u{9b}')
    {
        return text.to_owned();
    }
    let text = mask_control_split_tokens(text);
    mask_prefix_matches(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trailing_guard_follows_the_body_class() {
        assert_eq!(trailing_guard("ghp_[A-Za-z0-9]{10,}"), "(?![A-Za-z0-9])");
        assert_eq!(
            trailing_guard("glpat-[A-Za-z0-9_\\-]{10,}"),
            "(?![A-Za-z0-9_-])"
        );
        assert_eq!(trailing_guard("sk_[A-Za-z0-9_]{10,}"), "(?![A-Za-z0-9_])");
    }

    #[test]
    fn opaque_bodies_are_told_from_words() {
        assert!(is_opaque_prefixed_token("sk-abcdefghijklmnop1234567890"));
        assert!(!is_opaque_prefixed_token("sk-learn"));
        assert!(!is_opaque_prefixed_token("sk-prod-deployment-01"));
    }
}
