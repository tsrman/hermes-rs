//! Credentials in JSON text and in Python mapping `repr`: `"api_key": "…"` and
//! `{'API_KEY': '…'}`, including the escaped forms inside a JSON-encoded string.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

use super::assignments::{
    has_password_keyword, has_word_bounded_keyword, is_qualified_key_name, should_redact_assignment,
};
use super::mask::mask_token;
use super::tokens::replace_all;

const KEY_QUALIFIED_NAME: &str = r"[\w.\-]*(?:secret|private|signing|encryption|encrypt|master|hmac|symmetric|access)[_.\-]?(?:key|Key|KEY)";

fn json_key_names() -> String {
    format!(
        r"(?:api_?[Kk]ey|token|secret|password|pgpassword|mysql_pwd|sshpass|access_token|refresh_token|auth_token|bearer|secret_value|raw_secret|secret_input|key_material|client[_.\-]?key[_.\-]?data|{KEY_QUALIFIED_NAME})"
    )
}

/// `"name": "value"` for the closed list of credential names.
static JSON_FIELD: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(r#"(?i)("{}")\s*:\s*"([^"]+)""#, json_key_names())).expect("valid pattern")
});
/// The same inside a JSON-encoded string; the back reference keeps both escape depths equal.
static JSON_FIELD_ESCAPED: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r#"(?i)((\\+)"{}\2")\s*:\s*\2"([^"\\]+)\2""#,
        json_key_names()
    ))
    .expect("valid pattern")
});
/// Any identifier-shaped key; the callback applies the key policy.
static JSON_ANY_KEY: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r#"("((?>[A-Za-z0-9_.\-]{1,128}))")(?>\s*):(?>\s*)"((?>(?:[^"\\\n]|\\.)+))""#)
        .expect("valid pattern")
});
static JSON_ANY_KEY_ESCAPED: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r#"((\\+)"((?>[A-Za-z0-9_.\-]{1,128}))\2")(?>\s*):(?>\s*)\2"((?>[^"\\\n]+))\2""#)
        .expect("valid pattern")
});
static JSON_KEY_NAMES_FULL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)^{}$", json_key_names())).expect("valid pattern"));
static YAML_CFG_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:api[ _.\-]?key|token|secret|passwd|password|credential|client[ _.\-]?key[ _.\-]?data|(?:private|signing|encryption|master|hmac)[ _.\-]?key)",
    )
    .expect("valid pattern")
});
static DSN_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:<(?:[a-z][a-z_ -]*|[A-Z][A-Z_ -]*)>|\$\{?[A-Z_][A-Z0-9_]*\}?|%[A-Z_][A-Z0-9_]*%)$",
    )
    .expect("valid pattern")
});
static VALUE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[_.\-]?(?:value|Value|VALUE)$").expect("valid pattern"));

const REPR_SECRET_KEYS: [&str; 20] = [
    "access_token",
    "refresh_token",
    "id_token",
    "auth_token",
    "token",
    "api_key",
    "apikey",
    "client_secret",
    "secret",
    "password",
    "passwd",
    "private_key",
    "credential",
    "credentials",
    "authorization",
    "bearer",
    "secret_value",
    "raw_secret",
    "secret_input",
    "key_material",
];
const REPR_ENV_SUFFIXES: [&str; 7] = [
    "_API_KEY",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_PASSWD",
    "_CREDENTIAL",
    "_CREDENTIALS",
];
const REPR_CREDENTIAL_SUFFIXES: [&str; 8] = [
    "apikey",
    "api_key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "credentials",
];

/// Exact secret keys, credential-suffixed names, and qualified `…key` names. Metadata names
/// such as `TOKEN_COUNT` or `password_policy` stay out.
pub(super) fn is_python_repr_secret_key(key: &str) -> bool {
    let folded = key.to_lowercase();
    if REPR_SECRET_KEYS.contains(&folded.as_str()) {
        return true;
    }
    let upper = key.chars().any(char::is_alphabetic) && !key.chars().any(char::is_lowercase);
    if upper && REPR_ENV_SUFFIXES.iter().any(|suffix| key.ends_with(suffix)) {
        return true;
    }
    if REPR_CREDENTIAL_SUFFIXES
        .iter()
        .any(|suffix| folded.ends_with(suffix))
    {
        return true;
    }
    is_qualified_key_name(key)
}

/// Whether `"key": "value"` holds a credential under an affixed secret-word key.
fn json_affixed_key_secret(key: &str, value: &str) -> bool {
    // The closed-list pass already decided these fields.
    if JSON_KEY_NAMES_FULL.is_match(key) {
        return false;
    }
    if !(has_word_bounded_keyword(key, &YAML_CFG_WORD) || has_password_keyword(key)) {
        return false;
    }
    if DSN_PLACEHOLDER.is_match(value) || !should_redact_assignment(key, value, true) {
        return false;
    }
    // A name that does not end in the credential word is metadata about one
    // (`passwordChanged`, `tokenUrl`, `secret_id`); `api_key_value` holds the credential itself.
    is_python_repr_secret_key(&VALUE_SUFFIX.replace(key, ""))
}

/// `'key': 'value'` and `'key': b"value"` of a Python mapping.
static PYTHON_REPR_FIELD: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(
        r#"'(?P<key>[A-Za-z_][A-Za-z0-9_]*)'(?P<sep>\s*:\s*)(?:(?P<single_prefix>[bB]?)'(?P<single_value>(?:\\.|[^'\\])+)'|(?P<double_prefix>[bB]?)"(?P<double_value>(?:\\.|[^"\\])+)")"#,
    )
    .expect("valid pattern")
});

fn is_env_lookup(value: &str) -> bool {
    ["os.getenv", "os.environ", "process.env", "$ENV{"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

pub(super) fn redact_python_repr_fields(text: &str) -> String {
    if !(text.contains(':') && text.contains('\'')) {
        return text.to_owned();
    }
    replace_all(&PYTHON_REPR_FIELD, text, |caps| {
        let key = &caps["key"];
        if !is_python_repr_secret_key(key) {
            return caps[0].to_owned();
        }
        let (prefix, quote, value) = match caps.name("single_value") {
            Some(value) => (
                caps.name("single_prefix").map_or("", |m| m.as_str()),
                '\'',
                value.as_str(),
            ),
            None => (
                caps.name("double_prefix").map_or("", |m| m.as_str()),
                '"',
                caps.name("double_value").map_or("", |m| m.as_str()),
            ),
        };
        if is_env_lookup(value) || value.contains("***") || value.starts_with("«redacted:") {
            return caps[0].to_owned();
        }
        // No head or tail is kept: an escaped repr atom could cross the slicing boundary and
        // leave an unescaped quote behind.
        format!("'{key}'{}{prefix}{quote}***{quote}", &caps["sep"])
    })
}

pub(super) fn redact_json_fields(text: &str) -> String {
    let mut text = text.to_owned();
    if text.contains(':') && text.contains('"') {
        text = super::headers::redact_json_header_fields(&text);
        text = replace_all(&JSON_FIELD, &text, |caps| {
            let key = &caps[1];
            // The bare name `apiKey` is left as it is by the reference.
            if key[1..key.len() - 1].eq_ignore_ascii_case("apikey")
                || !should_redact_assignment(key, &caps[2], false)
            {
                return caps[0].to_owned();
            }
            format!("{key}: \"{}\"", mask_token(&caps[2]))
        });
        let escaped = text.contains("\\\"");
        if escaped {
            text = replace_all(&JSON_FIELD_ESCAPED, &text, |caps| {
                if !should_redact_assignment(&caps[1], &caps[3], false) {
                    return caps[0].to_owned();
                }
                format!(
                    "{}: {}\"{}{}\"",
                    &caps[1],
                    &caps[2],
                    mask_token(&caps[3]),
                    &caps[2]
                )
            });
        }
        text = replace_all(&JSON_ANY_KEY, &text, |caps| {
            if !json_affixed_key_secret(&caps[2], &caps[3]) {
                return caps[0].to_owned();
            }
            format!("{}: \"{}\"", &caps[1], mask_token(&caps[3]))
        });
        if text.contains("\\\"") {
            text = replace_all(&JSON_ANY_KEY_ESCAPED, &text, |caps| {
                if !json_affixed_key_secret(&caps[3], &caps[4]) {
                    return caps[0].to_owned();
                }
                format!(
                    "{}: {}\"{}{}\"",
                    &caps[1],
                    &caps[2],
                    mask_token(&caps[4]),
                    &caps[2]
                )
            });
        }
    }
    redact_python_repr_fields(&text)
}
