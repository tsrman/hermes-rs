//! `key = value` config assignments with spaces around the `=` (INI, TOML).
//!
//! That spacing is also the default style of Python, JavaScript and Go source, so the form is
//! masked only for config-shaped values: a quoted literal, or an unquoted last token that looks
//! opaque and is not a code expression.

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex as Fancy};
use regex::Regex;

use super::assignments::{looks_like_opaque_credential, should_redact_assignment};
use super::fields::is_python_repr_secret_key;
use super::mask::mask_token;
use super::tokens::replace_all;

const SECRET_CFG_NAMES: &str = r"(?:api[ _.\-]?key|token|secret|passwd|password|credential|auth)";
const LINE_NUMBER_GUTTER: &str = r"(?:[0-9]+(?:[|:\-]|\t)[ \t]*)?";
const SPACED_SEP: &str = r"(?:(?>[ \t]+)=(?![=>~])(?>[ \t]*)|=(?![=>~])(?>[ \t]+))";
const QUOTED: &str =
    r#"(?P<q>['"])(?P<qv>(?>(?:\\.|(?!(?P=q))[^\\\n])+))(?P=q)(?=(?>[ \t]*)(?:[;,#]|//|\r?$))"#;
const UNQUOTED: &str = r#"(?P<uv>(?>[^\s'"]+))(?=(?:(?>[ \t]+)[#;](?>[^\n]*))?(?>[ \t]*)\r?$)"#;
/// `api key` is the only name that holds a space.
const SP: &str = r"(?<=[Aa][Pp][Ii]) (?=[Kk][Ee][Yy])";

static SPACED_EQ: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]=|=[ \t]").expect("valid pattern"));
static CFG_SECRET_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("(?i){SECRET_CFG_NAMES}")).expect("valid pattern"));
static IDENTIFIER_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*$").expect("valid pattern")
});
static KEY_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\w$-]").expect("valid pattern"));
static NAME_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\w-]").expect("valid pattern"));

static DOTTED: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?m)(?<![A-Za-z0-9_.\-])(?!(?<=[Aa][Pp][Ii] )[Kk][Ee][Yy])(?P<key>(?P<name>(?>(?:[A-Za-z0-9_\-]|{SP})*)\.(?>(?:[A-Za-z0-9_.\-]|{SP})*))){SPACED_SEP}(?:{QUOTED}|{UNQUOTED})"
    ))
    .expect("valid pattern")
});
static ANCHORED: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?mi)(?P<key>^[ \t]*{LINE_NUMBER_GUTTER}(?:export[ \t]+)?(?P<name>(?>(?:[A-Za-z0-9_.\-]|{SP})+))){SPACED_SEP}(?:{QUOTED}|{UNQUOTED})"
    ))
    .expect("valid pattern")
});

/// Whether the key places a secret word the way the adjacent-form passes require.
fn key_names_secret(name: &str, dotted: bool) -> bool {
    let chars: Vec<char> = name.chars().collect();
    if !dotted {
        // Only `api.key` / `api key` puts a non-word character in a bare key.
        let separators: Vec<usize> = chars
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, '.' | ' '))
            .map(|(i, _)| i)
            .collect();
        if let Some(&first) = separators.first() {
            let before: String = chars[first.saturating_sub(3)..first]
                .iter()
                .collect::<String>()
                .to_lowercase();
            let after: String = chars[first + 1..(first + 4).min(chars.len())]
                .iter()
                .collect::<String>()
                .to_lowercase();
            if separators.len() > 1 || before != "api" || after != "key" {
                return false;
            }
        }
        return CFG_SECRET_WORD.is_match(name);
    }
    let byte_of = |position: usize| {
        name.char_indices()
            .nth(position)
            .map_or(name.len(), |(b, _)| b)
    };
    let first_dot = chars.iter().position(|c| *c == '.');
    let last_dot = chars[..chars.len().saturating_sub(1)]
        .iter()
        .rposition(|c| *c == '.');
    first_dot.is_some_and(|d| d > 0 && CFG_SECRET_WORD.is_match(&name[byte_of(d + 1)..]))
        || last_dot.is_some_and(|d| d > 0 && CFG_SECRET_WORD.is_match(&name[..byte_of(d)]))
}

/// Code, not config: a call, subscript, literal keyword, the key's own name, a value that does
/// not look opaque, or an identifier path.
fn is_code_expression(key: &str, value: &str) -> bool {
    let folded = value.to_lowercase();
    if value.contains(['(', '[', '{'])
        || value.ends_with(';')
        || ["none", "null", "true", "false", "nil", "undefined"].contains(&folded.as_str())
    {
        return true;
    }
    if KEY_SEGMENT
        .split(key)
        .last()
        .is_some_and(|last| last.to_lowercase() == folded)
    {
        return true;
    }
    if !looks_like_opaque_credential(value) {
        return true;
    }
    IDENTIFIER_PATH.is_match(value)
        && (value.contains('.') || !value.chars().any(|c| c.is_ascii_digit()))
}

/// A quoted literal is masked under a key that names a credential; under a key that only
/// mentions one, only an opaque single token is.
fn quoted_value_is_secret(key: &str, value: &str) -> bool {
    let name = NAME_SEGMENT
        .split(key)
        .last()
        .unwrap_or("")
        .replace('-', "_");
    if value.to_lowercase() == name.to_lowercase() {
        return false;
    }
    is_python_repr_secret_key(&name)
        || (!value.chars().any(char::is_whitespace) && looks_like_opaque_credential(value))
}

fn replace_pass(pattern: &Fancy, text: &str, dotted: bool) -> String {
    replace_all(pattern, text, |caps: &Captures<'_, str>| {
        let name = caps.name("name").map_or("", |m| m.as_str());
        if !key_names_secret(name, dotted) {
            return caps[0].to_owned();
        }
        let key = caps.name("key").map_or("", |m| m.as_str());
        let quoted = caps.name("qv");
        let value_match = quoted.or_else(|| caps.name("uv"));
        let Some(value_match) = value_match else {
            return caps[0].to_owned();
        };
        let value = value_match.as_str();
        let secret = if quoted.is_some() {
            quoted_value_is_secret(key, value)
        } else {
            !is_code_expression(key, value)
        };
        if !secret || !should_redact_assignment(key, value, true) {
            return caps[0].to_owned();
        }
        let origin = caps.get(0).map_or(0, |m| m.start());
        let whole = &caps[0];
        format!(
            "{}{}{}",
            &whole[..value_match.start() - origin],
            mask_token(value),
            &whole[value_match.end() - origin..]
        )
    })
}

/// Mask config secrets written `key = value`; the adjacent `key=value` form is not matched here.
pub(super) fn redact_spaced_cfg_assignments(text: &str) -> String {
    if !SPACED_EQ.is_match(text) {
        return text.to_owned();
    }
    let text = replace_pass(&DOTTED, text, true);
    replace_pass(&ANCHORED, &text, false)
}

/// The gate of the reference: no `://` and some secret word in the text.
pub(super) fn applies(text: &str) -> bool {
    !text.contains("://") && CFG_SECRET_WORD.is_match(text)
}
