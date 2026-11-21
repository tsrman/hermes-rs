//! Config-file assignments: namespaced keys (`spring.datasource.password=x`), keys at the
//! start of a line (`password=x`, `export api_key=x`) and YAML `key: value` lines.
//!
//! They run only on text without `://`: web-URL query parameters are passed through on purpose.
//! The spaced form `key = value` is not ported yet.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

use super::assignments::should_redact_assignment;
use super::mask::mask_token;
use super::tokens::replace_all;

const SECRET_CFG_NAMES: &str = r"(?:api[ _.\-]?key|token|secret|passwd|password|credential|auth)";
const LINE_NUMBER_GUTTER: &str = r"(?:[0-9]+(?:[|:\-]|\t)[ \t]*)?";
const YAML_CFG_NAMES: &str = r"(?:api[ _.\-]?key|token|secret|passwd|password|credential|client[ _.\-]?key[ _.\-]?data|(?:private|signing|encryption|master|hmac)[ _.\-]?key)";
const CFG_VALUE: &str = r#"(['"]?)([^\s&]+?)\2(?=[\s&]|$)"#;

static CFG_SECRET_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("(?i){SECRET_CFG_NAMES}")).expect("valid pattern"));
/// A namespaced key: the secret word may sit anywhere in a dotted path.
static CFG_DOTTED: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?i)(?<![A-Za-z0-9_.\-])((?>[A-Za-z0-9_\-]+)\.[A-Za-z0-9_.\-]*{SECRET_CFG_NAMES}(?>[A-Za-z0-9_.\-]*)|[A-Za-z0-9_.\-]*{SECRET_CFG_NAMES}[A-Za-z0-9_.\-]*\.(?>[A-Za-z0-9_.\-]+))={CFG_VALUE}"
    ))
    .expect("valid pattern")
});
/// A bare key at the start of a line, optionally after `export` or a rendered line number.
static CFG_ANCHORED: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?mi)(^[ \t]*{LINE_NUMBER_GUTTER}(?:export[ \t]+)?[A-Za-z0-9_\-]*{SECRET_CFG_NAMES}[A-Za-z0-9_\-]*)={CFG_VALUE}"
    ))
    .expect("valid pattern")
});
static YAML_ASSIGN: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r#"(?mi)(^(?>[ \t]*){LINE_NUMBER_GUTTER}[A-Za-z0-9_.\-]*{YAML_CFG_NAMES}(?>[A-Za-z0-9_.\-]*))(:(?>[ \t]*))("(?:\\[^\r\n]|[^"\\\r\n])*"|'(?:''|[^'\r\n])*'|['"][^\r\n]*|(?>[^\s&]+))"#
    ))
    .expect("valid pattern")
});
static DSN_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:<(?:[a-z][a-z_ -]*|[A-Z][A-Z_ -]*)>|\$\{?[A-Z_][A-Z0-9_]*\}?|%[A-Z_][A-Z0-9_]*%)$",
    )
    .expect("valid pattern")
});

fn redact_assignment(caps: &fancy_regex::Captures<'_, str>) -> String {
    let (key, quote, value) = (&caps[1], &caps[2], &caps[3]);
    if !should_redact_assignment(key, value, true) {
        return caps[0].to_owned();
    }
    format!("{key}={quote}{}{quote}", mask_token(value))
}

fn redact_yaml(caps: &fancy_regex::Captures<'_, str>) -> String {
    let (key, separator, value) = (&caps[1], &caps[2], &caps[3]);
    let chars: Vec<char> = value.chars().collect();
    let quote =
        if chars.len() >= 2 && matches!(chars[0], '"' | '\'') && chars[chars.len() - 1] == chars[0]
        {
            Some(chars[0])
        } else {
            None
        };
    let inner: String = if quote.is_some() {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        value.to_owned()
    };
    // An empty or docs-placeholder scalar, or a Helm/Jinja template, is not a credential.
    if quote.is_some()
        && (inner.is_empty()
            || DSN_PLACEHOLDER.is_match(&inner)
            || (inner.starts_with("{{") && inner.ends_with("}}")))
    {
        return caps[0].to_owned();
    }
    if !should_redact_assignment(key, &inner, true) {
        return caps[0].to_owned();
    }
    let q = quote.map(String::from).unwrap_or_default();
    format!("{key}{separator}{q}{}{q}", mask_token(&inner))
}

/// The dotted, anchored and YAML passes. The text must not contain `://`.
pub(super) fn redact_config_assignments(text: &str) -> String {
    let mut text = text.to_owned();
    if !text.contains("://") && CFG_SECRET_WORD.is_match(&text) {
        text = replace_all(&CFG_DOTTED, &text, redact_assignment);
        text = replace_all(&CFG_ANCHORED, &text, redact_assignment);
    }
    text
}

/// YAML lines come after the JSON and repr passes.
pub(super) fn redact_yaml_assignments(text: &str) -> String {
    if text.contains(':') && !text.contains("://") {
        return replace_all(&YAML_ASSIGN, text, redact_yaml);
    }
    text.to_owned()
}
