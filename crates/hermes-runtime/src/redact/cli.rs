//! Credentials written as command-line flags (`--password x`, `docker login -p x`) and as
//! cloud connection-string keys (`AccountKey=…`).

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex as Fancy};
use regex::Regex;

use super::assignments::{opaque_path_segment, password_key};
use super::mask::mask_token;
use super::tokens::replace_all;

/// A quoted value honours escapes and the shell's `'\''` splice; an unterminated quote masks
/// to the end of the line. The reference's possessive bodies are atomic groups here.
const QUOTED_FLAG_VALUE: &str =
    r#""(?>(?:\\[\s\S]|[^"\\]){1,512})"|'(?>(?:'\\''|[^']){1,512})'|["'](?>[^\n]*)|\S+"#;

static CLI_SECRET_FLAG: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?i)(?P<flag>(?<![\w-])--?(?:password|passwd|passphrase|pass|token|api[-_]?key|apikey|client[-_]?secret|access[-_]?key|account[-_]?key|auth[-_]?token|session[-_]?token|secret|bearer|credential)s?)(?P<sep>\s*=|\s+)(?P<value>{QUOTED_FLAG_VALUE})"
    ))
    .expect("valid pattern")
});
static CLI_LOGIN_P: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?P<flag>\b(?:docker|podman|nerdctl)\s+login\b[^\n;|&]*?\s-p\s*)(?P<value>{QUOTED_FLAG_VALUE})"
    ))
    .expect("valid pattern")
});
const CONNSTR_PREFIX: &str = r#"(?P<flag>(?<![^;,?&'"\s])(?:{KEYS})(?:=|[ \t]*(?>=)[ \t]*(?=[A-Za-z+/]*[0-9])(?=[A-Za-z0-9+/]{16,}={0,2}(?![\w.(]))))"#;
static CONNSTR_KEY: LazyLock<Fancy> = LazyLock::new(|| {
    let prefix = CONNSTR_PREFIX.replace("{KEYS}", "AccountKey|SharedAccessKey");
    Fancy::new(&format!(r#"(?i){prefix}(?P<value>[^;\s'"&]+)"#)).expect("valid pattern")
});
/// A SAS value is itself a query string: it runs through its `sig=` parameter.
static CONNSTR_SAS: LazyLock<Fancy> = LazyLock::new(|| {
    let prefix = CONNSTR_PREFIX.replace("{KEYS}", "SharedAccessSignature");
    Fancy::new(&format!(
        r#"(?i){prefix}(?P<value>(?>(?:(?!sig=)[^;\s'"&),`|]+&)*)(?:sig=)?(?>[^;\s'"&),`|]*))"#
    ))
    .expect("valid pattern")
});
static CLI_GATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-{1,2}[A-Za-z]").expect("valid pattern"));
static LOGIN_GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:docker|podman|nerdctl)\s+login\b").expect("valid pattern")
});
static CONNSTR_GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)AccountKey|SharedAccessKey|SharedAccessSignature").expect("valid pattern")
});
static REDACTION_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\*{3}|«redacted(?::[^»\s]*|-secret)»|\[REDACTED(?: PRIVATE KEY\])?)$")
        .expect("valid pattern")
});
/// An unquoted space-separated value that is a word (a metavar or a prose word), not a credential.
static FLAG_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:[A-Z][A-Z_]*|[a-z]{1,8}|[a-z]{1,8}_[a-z]{1,8}|[a-z]{1,8}[A-Z][a-z]{1,8})[\]).,:;]*|[-\[(<].*|[a-z][\w-]*=.*|\S*(?:xxx|XXX|\.\.\.))$",
    )
    .expect("valid pattern")
});

fn is_flag_placeholder(value: &str) -> bool {
    !value.starts_with(['\'', '"']) && FLAG_PLACEHOLDER.is_match(value)
}

/// `--credentials ~/.config/x/creds.json` names a file: only password-class flags mask a path,
/// and only while no segment looks opaque.
fn is_flag_path_reference(flag: &str, value: &str) -> bool {
    if password_key(flag)
        || !["/", "~", "./", "../"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        return false;
    }
    !value.split('/').any(opaque_path_segment)
}

fn is_redaction_marker(text: &str, caps: &Captures<'_, str>, value: &str) -> bool {
    if value == "[REDACTED" {
        let end = caps.name("value").map_or(0, |m| m.end());
        return text[end..].starts_with(" PRIVATE KEY]");
    }
    REDACTION_MARKER.is_match(value)
}

/// Mask the value of a flag or connection-string key, keeping the flag and dropping the quotes.
fn mask_flag_value(text: &str, caps: &Captures<'_, str>, is_flag: bool) -> String {
    let whole = caps[0].to_owned();
    let Some(value_match) = caps.name("value") else {
        return whole;
    };
    let value = value_match.as_str();
    let stripped = value.trim_matches(['\'', '"']);
    let first = stripped.chars().next();
    if stripped.is_empty() || is_redaction_marker(text, caps, stripped.trim()) {
        return whole;
    }
    // `--password=$DB_PASS` names a variable; masking it would make the command unreplayable.
    if first.is_some_and(|c| matches!(c, '$' | '<' | '{' | '%')) {
        return whole;
    }
    // `--token 60` is a timeout.
    if stripped.chars().all(|c| c.is_numeric()) {
        return whole;
    }
    if is_flag {
        let sep = caps.name("sep").map_or("", |m| m.as_str());
        if !sep.contains('=') && is_flag_placeholder(value) {
            return whole;
        }
        if is_flag_path_reference(caps.name("flag").map_or("", |m| m.as_str()), stripped) {
            return whole;
        }
    }
    let start = value_match.start() - caps.get(0).map_or(0, |m| m.start());
    let end = value_match.end() - caps.get(0).map_or(0, |m| m.start());
    format!(
        "{}{}{}",
        &whole[..start],
        mask_token(stripped),
        &whole[end..]
    )
}

pub(super) fn redact_cli_secrets(text: &str) -> String {
    let mut text = text.to_owned();
    if text.contains('=') || text.contains(' ') {
        if CLI_GATE.is_match(&text) {
            let source = text.clone();
            text = replace_all(&CLI_SECRET_FLAG, &source, |caps| {
                mask_flag_value(&source, caps, true)
            });
        }
        if LOGIN_GATE.is_match(&text) {
            let source = text.clone();
            text = replace_all(&CLI_LOGIN_P, &source, |caps| {
                mask_flag_value(&source, caps, false)
            });
        }
    }
    if CONNSTR_GATE.is_match(&text) {
        for pattern in [&*CONNSTR_KEY, &*CONNSTR_SAS] {
            let source = text.clone();
            text = replace_all(pattern, &source, |caps| {
                mask_flag_value(&source, caps, false)
            });
        }
    }
    text
}
