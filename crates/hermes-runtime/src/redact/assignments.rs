//! `NAME=value` assignments whose name says that the value is a credential.
//!
//! The key and value tests are shared by the other assignment passes (JSON, YAML, config
//! files), which are not ported yet.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

use super::mask::mask_token;
use super::tokens::replace_all;

const SECRET_ENV_NAMES: &str =
    r"(?:API_?KEY|KEY|TOKEN|SECRET|PASSWORD|PASSWD|PASS|PW|CREDENTIAL|AUTH)";

/// Uppercase names tolerate spaces around `=` and the keyword anywhere (`MYTOKEN=…`).
static ENV_ASSIGN: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r#"([A-Z0-9_]{{0,50}}{SECRET_ENV_NAMES}[A-Z0-9_]{{0,50}})\s*=\s*(['"]?)(\S+)\2"#
    ))
    .expect("valid pattern")
});

/// Lowercase names count only with an underscore boundary (`openai_key=`), never a bare
/// `password=` or `token=`, which turn up in prose and form bodies.
static ENV_ASSIGN_LOWER: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(
        r#"(?i)(?<![a-z0-9_])([a-z0-9_]+(?:_|^)(?:key|pass|pw|token|secret|password|passwd|credential|auth)(?=[^a-z0-9_]|$))\s*=\s*(['"]?)(\S+)\2"#,
    )
    .expect("valid pattern")
});

static KEY_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:api|auth|access|refresh|session|secret)[ _.\\-]?(?:key|token)|token|secret|passwd|password|pass|pw|credential|auth|key",
    )
    .expect("valid pattern")
});
static STRONG_KEY_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:api|auth|access|refresh|session|id|bearer)[ _.\\-]?(?:key|token)|key[ _.\\-]?material|secret|passwd|password|pass|pw|credential|auth|bearer",
    )
    .expect("valid pattern")
});
static PASSWORD_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)passwd|password|pass|pw").expect("valid pattern"));
static PASSWORD_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)passwd|password").expect("valid pattern"));
static GLUED_CREDENTIAL_ENV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:PGPASSWORD|MYSQL_PWD|SSHPASS)$").expect("valid pattern"));
static ENV_LOOKUP_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:os\.(?:getenv|environ)|process\.env|\$ENV\{)").expect("valid pattern")
});
static HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Fa-f0-9]+$").expect("valid pattern"));
static URLSAFE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./+=-]+$").expect("valid pattern"));
const SHELL_VAR_REF: &str = r"\$(?:\{[A-Za-z_]\w*[^}]*\}|[A-Za-z_]\w*)";
static PATH_OR_VAR_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^(?:{SHELL_VAR_REF}|\$\(|~|/)(?:[\w./:-]|{SHELL_VAR_REF})*$"
    ))
    .expect("valid pattern")
});
static OPAQUE_PATH_SEGMENT: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r"^(?=[^.]*[a-z])(?=[^.]*[A-Z])(?=[^.]*[0-9])[^.]{16,}$").expect("valid pattern")
});
static CONFIG_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:<[a-z]+(?:[ _-][a-z]+)+>|<(?:key|token|secret|password|value)>|true|false|yes|no|null|none|default)$")
        .expect("valid pattern")
});

const CONFIG_LITERALS: [&str; 8] = ["true", "false", "yes", "no", "on", "off", "null", "none"];
const KEY_SECRET_QUALIFIERS: [&str; 8] = [
    "secret",
    "private",
    "signing",
    "encryption",
    "encrypt",
    "master",
    "hmac",
    "symmetric",
];

/// Position `i` begins a word: the edge, a non-letter before it, or a camelCase boundary.
fn is_word_start(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let (prev, cur) = (chars[i - 1], chars[i]);
    if !prev.is_alphabetic() || (cur.is_uppercase() && prev.is_lowercase()) {
        return true;
    }
    cur.is_uppercase()
        && prev.is_uppercase()
        && chars.get(i + 1).is_some_and(|next| next.is_lowercase())
}

/// Position `j` (exclusive) ends a word; one trailing `s` is absorbed.
fn is_word_end(chars: &[char], j: usize, allow_plural: bool) -> bool {
    if j >= chars.len() {
        return true;
    }
    let cur = chars[j];
    if !cur.is_alphabetic() || (cur.is_uppercase() && chars[j - 1].is_lowercase()) {
        return true;
    }
    allow_plural && matches!(cur, 's' | 'S') && is_word_end(chars, j + 1, false)
}

fn has_word_bounded_keyword(key: &str, keyword: &Regex) -> bool {
    let chars: Vec<char> = key.chars().collect();
    let char_of = |byte: usize| key[..byte].chars().count();
    keyword.find_iter(key).any(|m| {
        is_word_start(&chars, char_of(m.start())) && is_word_end(&chars, char_of(m.end()), true)
    })
}

/// `password` or `passwd` ending a word, wherever it starts (`dbpassword`, not `passwordless`).
fn has_password_keyword(key: &str) -> bool {
    let chars: Vec<char> = key.chars().collect();
    PASSWORD_WORD
        .find_iter(key)
        .any(|m| is_word_end(&chars, key[..m.end()].chars().count(), true))
}

fn is_glued_credential_env(key: &str) -> bool {
    GLUED_CREDENTIAL_ENV.is_match(key.trim_matches(['"', '\'']))
}

/// `API_KEY` and `DB_PW` count, `KEYBOARD` and `secretary` do not.
fn key_has_secret_keyword(key: &str) -> bool {
    has_word_bounded_keyword(key, &KEY_KEYWORD)
        || has_password_keyword(key)
        || is_glued_credential_env(key)
}

/// A credential-like shape, so that short technical scalars are not masked for their key name.
fn looks_like_opaque_credential(value: &str) -> bool {
    if value == "***" || value.starts_with("«redacted:") {
        return true;
    }
    let length = value.chars().count();
    if length >= 16 && HEX.is_match(value) {
        return true;
    }
    if length >= 20 && URLSAFE.is_match(value) {
        return true;
    }
    if length < 12 {
        return false;
    }
    let kinds = [
        value.chars().any(|c| c.is_ascii_lowercase()),
        value.chars().any(|c| c.is_ascii_uppercase()),
        value.chars().any(|c| c.is_ascii_digit()),
    ];
    kinds.iter().filter(|found| **found).count() >= 2
}

/// The words of a field name: `aws_secret_access_key` and `awsSecretAccessKey` alike.
fn key_name_words(key: &str) -> Vec<String> {
    let mut spaced = String::new();
    let mut previous: Option<char> = None;
    for c in key.chars() {
        if c.is_ascii_uppercase()
            && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
        {
            spaced.push('_');
        }
        spaced.push(c);
        previous = Some(c);
    }
    spaced
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A `…key` name whose qualifier (`secret`, `private`, `signing`, …) makes it a credential.
fn is_qualified_key_name(key: &str) -> bool {
    let words = key_name_words(key);
    if words.len() < 2 || words.last().map(String::as_str) != Some("key") {
        return false;
    }
    words[..words.len() - 1]
        .iter()
        .any(|word| KEY_SECRET_QUALIFIERS.contains(&word.as_str()) || word == "access")
}

/// The shared gate of the assignment passes: should this key and value be masked?
pub(super) fn should_redact_assignment(key: &str, value: &str, check_keyword: bool) -> bool {
    // A programmatic lookup names a variable; it is not a leaked value.
    if ENV_LOOKUP_VALUE.is_match(value) {
        return false;
    }
    // An earlier pass already masked it.
    if value == "***" || value.starts_with("«redacted") {
        return false;
    }
    if CONFIG_LITERALS.contains(&value.to_lowercase().as_str()) {
        return false;
    }
    if check_keyword && !key_has_secret_keyword(key) {
        return false;
    }
    let password_key = has_word_bounded_keyword(key, &PASSWORD_KEY) || has_password_keyword(key);
    if PATH_OR_VAR_VALUE.is_match(value) && !password_key {
        // `$VAR` is an unambiguous reference. A path counts as one only while every segment
        // reads like a path: a long segment mixing case and digits is a secret.
        let opaque_segment = value
            .split('/')
            .any(|segment| OPAQUE_PATH_SEGMENT.is_match(segment).unwrap_or(false));
        if value.starts_with('$') || !opaque_segment {
            return false;
        }
    }
    password_key
        || has_word_bounded_keyword(key, &STRONG_KEY_KEYWORD)
        || is_glued_credential_env(key)
        || (is_qualified_key_name(key) && !CONFIG_PLACEHOLDER.is_match(value))
        || looks_like_opaque_credential(value)
}

fn mask_assignment(caps: &fancy_regex::Captures<'_, str>) -> String {
    let (key, quote, value) = (&caps[1], &caps[2], &caps[3]);
    if !should_redact_assignment(key, value, true) {
        return caps[0].to_owned();
    }
    format!("{key}={quote}{}{quote}", mask_token(value))
}

/// The uppercase and lowercase `NAME=value` passes. Cookie and spaced-config assignments
/// come first in the reference and are not ported.
pub(super) fn redact_env_assignments(text: &str) -> String {
    if !text.contains('=') {
        return text.to_owned();
    }
    let text = replace_all(&ENV_ASSIGN, text, mask_assignment);
    // Lowercase names would match URL query parameters, which are passed through on purpose.
    if text.contains("://") {
        return text;
    }
    replace_all(&ENV_ASSIGN_LOWER, &text, mask_assignment)
}
