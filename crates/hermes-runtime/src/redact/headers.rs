//! Credential-bearing HTTP headers: API-key style headers, `Cookie` and `Set-Cookie`, in raw
//! lines, `curl -H` arguments, log text and JSON-serialised request dumps, plus `cookie=`
//! assignments.

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex as Fancy};
use regex::Regex;

use super::assignments::should_redact_assignment;
use super::mask::mask_token;
use super::tokens::replace_all;

const SECRET_HEADER_NAMES: &str =
    r"(?:x-api-key|x-goog-api-key|api-key|apikey|x-api-token|x-auth-token|x-access-token)";
const CURL_HEADER_OPENER: &str =
    r"(?:^|[ \t])(?:-H(?:(?:[ \t]|\\\r?\n)*|=)|--header(?:(?:[ \t]|\\\r?\n)+|=))";
const COOKIE_NAME_CHARS: &str = r"[!#$%&'*+\-.^_`|~0-9A-Za-z]";

fn compile(pattern: &str) -> Fancy {
    Fancy::new(pattern).expect("valid header pattern")
}

static SECRET_HEADER_LINE: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r"(?mi)(^[ \t]*(?:[<>][ \t]+)?{SECRET_HEADER_NAMES}[ \t]*:[ \t]*)([^\r\n]+)"
    ))
});
static SECRET_INLINE: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r#"(?i)({SECRET_HEADER_NAMES}[ \t]*:[ \t]*)((?:[<(&;|]+(?![\s;&|<>()]|$))?(?:[^\s"';&|<>()`\\]|[;&|](?![\s;&|<>()]|$))+)"#
    ))
});
static SECRET_DOUBLE_QUOTED: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r#"(?i)({CURL_HEADER_OPENER}")({SECRET_HEADER_NAMES}:[ \t]*)((?:\\\r?\n|\\.|[^"\\\r\n])+)(")"#
    ))
});
static SECRET_SINGLE_QUOTED: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r"(?i)({CURL_HEADER_OPENER}')({SECRET_HEADER_NAMES}:[ \t]*)([^'\r\n]+)(')"
    ))
});
static SECRET_UNQUOTED_CURL: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r"(?mi)({CURL_HEADER_OPENER}{SECRET_HEADER_NAMES}:)((?:\\[^\r\n]|[^ \t\r\n;&|<>()$`\\])+)"
    ))
});
static COOKIE_LINE: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r"(?mi)(^[ \t]*(?:[<>][ \t]+)?(?:Set-)?Cookie:[ \t]*)({COOKIE_NAME_CHARS}+=[^\r\n]*)"
    ))
});
static COOKIE_DOUBLE_QUOTED: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r#"(?i)({CURL_HEADER_OPENER}")((?:Set-)?Cookie:[ \t]*)({COOKIE_NAME_CHARS}+=(?:\\\r?\n|\\.|[^"\\\r\n])*)(")"#
    ))
});
static COOKIE_SINGLE_QUOTED: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r"(?i)({CURL_HEADER_OPENER}')((?:Set-)?Cookie:[ \t]*)({COOKIE_NAME_CHARS}+=[^'\r\n]*)(')"
    ))
});
static COOKIE_UNQUOTED_CURL: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r"(?mi)({CURL_HEADER_OPENER}(?:Set-)?Cookie:[ \t]*)({COOKIE_NAME_CHARS}+=(?:\\[^\r\n]|[^ \t\r\n;&|<>()$`\\])*)"
    ))
});
/// A cookie header inside a line. `Set-Cookie:` also masks a bare token of 16 or more
/// characters; a plain `Cookie:` needs `name=value` pairs.
static COOKIE_INLINE: LazyLock<Fancy> = LazyLock::new(|| {
    let pair =
        format!(r#"{COOKIE_NAME_CHARS}+=(?:"[^"\r\n]*"|(?:[^\s;\\"'`]|'(?![\s'"`;&|<>()]|$))*)"#);
    compile(&format!(
        r#"(?i)(\bSet-Cookie:[ \t]*)(?:{pair}(?:;[ \t]*{pair})*|(?>[^\s\\"';&|<>()$`*=]{{16,}})(?![^\s;]))|(\bCookie:[ \t]*)(?:{pair}(?:;[ \t]*{pair})*)"#
    ))
});
static SECRET_JSON: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r#"(?i)("{SECRET_HEADER_NAMES}"\s*:\s*")((?:\\[^\r\n]|[^"\\\r\n])+)(")"#
    ))
});
static SECRET_JSON_UNTERMINATED: LazyLock<Fancy> = LazyLock::new(|| {
    compile(&format!(
        r#"(?mi)("{SECRET_HEADER_NAMES}"\s*:\s*")((?:\\[^\r\n]|[^"\\\r\n])*(?:\\)?)(?=\r?\n|\z)"#
    ))
});
static COOKIE_JSON: LazyLock<Fancy> =
    LazyLock::new(|| compile(r#"(?i)("(?:Set-)?Cookie"\s*:\s*")((?:\\[^\r\n]|[^"\\\r\n])*)(")"#));
static COOKIE_JSON_UNTERMINATED: LazyLock<Fancy> = LazyLock::new(|| {
    compile(r#"(?mi)("(?:Set-)?Cookie"\s*:\s*")((?:\\[^\r\n]|[^"\\\r\n])*(?:\\)?)(?=\r?\n|\z)"#)
});
static COOKIE_ASSIGN: LazyLock<Fancy> = LazyLock::new(|| {
    compile(concat!(
        r"(?i)(?<![\w?&])((?>(?:[A-Za-z0-9]+_)*)cookie\s*=(?!=)\s*)",
        r#"(?:(?P<q>["'])(?P<quoted>(?:(?!(?P=q))[^\\\r\n]|\\.)*)(?P=q)"#,
        r#"(?P<tail>(?>(?:\\.|'(?>[^'\r\n]*)'|"(?>[^"\s\\,:{}\[\]]*)"|(?>[^\s"'\;&|<>(),]+))*))"#,
        r#"|(?P<bare>(?>(?:[^\s"'`\;,&|<>()]|&(?![&\s]|$)|;(?=[!#$%'*+\-.^_|~0-9A-Za-z]+=))+)))"#
    ))
});
static MASKED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\*{3}|«redacted(?::[^»\s]*|-secret)»|\[REDACTED\])(?:\s|$)")
        .expect("valid pattern")
});
static BEFORE_CURL_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:-H|--header)(?:[ \t]*|=)["']?$"#).expect("valid pattern")
});
static CONTINUED_CURL_ARGUMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[ \t])(?:-H|--header)(?:[ \t]+|=).*\\[ \t]*$").expect("valid pattern")
});
static SHELL_VAR_WHOLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\$(?:\{[A-Za-z_]\w*[^}]*\}|[A-Za-z_]\w*)$").expect("valid pattern")
});

/// Mask an inline header token without taking the URL of an empty curl header.
fn redact_inline_secret_header(text: &str, caps: &Captures<'_, str>) -> String {
    let start = caps.get(0).map_or(0, |m| m.start());
    let window = text[..start]
        .char_indices()
        .rev()
        .nth(31)
        .map_or(0, |(i, _)| i);
    if BEFORE_CURL_FLAG.is_match(&text[window..start]) {
        return caps[0].to_owned();
    }
    format!("{}***", &caps[1])
}

/// Mask a header line unless it continues the argument of `curl -H`.
fn redact_header_line_value(text: &str, caps: &Captures<'_, str>) -> String {
    let start = caps.get(0).map_or(0, |m| m.start());
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    if line_start > 0 {
        let previous_start = text[..line_start - 1].rfind('\n').map_or(0, |i| i + 1);
        let previous = text[previous_start..line_start - 1].trim_end_matches('\r');
        if previous.trim_end().ends_with('\\') && CONTINUED_CURL_ARGUMENT.is_match(previous) {
            return caps[0].to_owned();
        }
    }
    // An earlier pass already masked the value: re-masking would make the pass non-idempotent.
    if MASKED_VALUE.is_match(&caps[2]) {
        return caps[0].to_owned();
    }
    format!("{}***", &caps[1])
}

/// Replace with a callback that also sees the text being scanned.
fn sub(
    pattern: &Fancy,
    text: &str,
    replacement: impl Fn(&str, &Captures<'_, str>) -> String,
) -> String {
    replace_all(pattern, text, |caps| replacement(text, caps))
}

/// Secret-header and cookie-header values in raw, curl and inline forms.
pub(super) fn redact_header_credentials(text: &str) -> String {
    let mut text = sub(&SECRET_HEADER_LINE, text, redact_header_line_value);
    text = sub(&SECRET_DOUBLE_QUOTED, &text, |_, caps| {
        format!("{}{}***{}", &caps[1], &caps[2], &caps[4])
    });
    text = sub(&SECRET_SINGLE_QUOTED, &text, |_, caps| {
        format!("{}{}***{}", &caps[1], &caps[2], &caps[4])
    });
    text = sub(&SECRET_UNQUOTED_CURL, &text, |_, caps| {
        format!("{}***", &caps[1])
    });
    text = sub(&SECRET_INLINE, &text, redact_inline_secret_header);
    text = sub(&COOKIE_LINE, &text, redact_header_line_value);
    for pattern in [&*COOKIE_DOUBLE_QUOTED, &*COOKIE_SINGLE_QUOTED] {
        text = sub(pattern, &text, |_, caps| {
            format!("{}{}***{}", &caps[1], &caps[2], &caps[4])
        });
    }
    text = sub(&COOKIE_UNQUOTED_CURL, &text, |_, caps| {
        format!("{}***", &caps[1])
    });
    sub(&COOKIE_INLINE, &text, |_, caps| {
        let prefix = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str());
        format!("{prefix}***")
    })
}

/// A cookie value is a credential when it is a `name=value` pair or an opaque token; a
/// qualified `SESSION_COOKIE` is as strong as `SESSION_TOKEN`.
fn cookie_value_is_credential(value: &str, qualified: bool) -> bool {
    if value.starts_with("«redacted") {
        return false;
    }
    let key = if qualified { "session_token" } else { "cookie" };
    (value.contains('=') && !value.starts_with('$')) || should_redact_assignment(key, value, false)
}

/// Whether a JSON `"<header>": "<value>"` field holds a credential.
fn json_header_value_is_credential(key: &str, value: &str) -> bool {
    if key.to_lowercase().contains("cookie") {
        return cookie_value_is_credential(value, false);
    }
    const LOOKUPS: [&str; 4] = ["os.getenv", "os.environ", "process.env", "$ENV{"];
    !(value == "***"
        || value.starts_with("«redacted")
        || LOOKUPS.iter().any(|prefix| value.starts_with(prefix))
        || SHELL_VAR_WHOLE.is_match(value))
}

fn decode_json_string(encoded: &str) -> String {
    serde_json::from_str::<String>(&format!("\"{encoded}\"")).unwrap_or_else(|_| encoded.to_owned())
}

fn encode_json_string(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_default();
    quoted[1..quoted.len() - 1].to_owned()
}

/// `"x-api-key": "…"` and `"Cookie": "…"` in JSON text, closed and truncated at a line end.
pub(super) fn redact_json_header_fields(text: &str) -> String {
    let mut text = text.to_owned();
    for pattern in [&*SECRET_JSON, &*COOKIE_JSON] {
        text = replace_all(pattern, &text, |caps| {
            let value = decode_json_string(&caps[2]);
            if !json_header_value_is_credential(&caps[1], &value) {
                return caps[0].to_owned();
            }
            format!(
                "{}{}{}",
                &caps[1],
                encode_json_string(&mask_token(&value)),
                &caps[3]
            )
        });
    }
    // A truncated value fails closed through the end of the line.
    for pattern in [&*SECRET_JSON_UNTERMINATED, &*COOKIE_JSON_UNTERMINATED] {
        text = replace_all(pattern, &text, |caps| {
            let value = decode_json_string(&caps[2]);
            format!("{}{}", &caps[1], encode_json_string(&mask_token(&value)))
        });
    }
    text
}

/// `cookie=` and `*_COOKIE=` assignments; a shell word that joins quoted and bare parts is
/// masked as one word.
pub(super) fn redact_cookie_assignments(text: &str) -> String {
    replace_all(&COOKIE_ASSIGN, text, |caps| {
        let key = &caps[1];
        let tail = caps.name("tail").map_or("", |m| m.as_str());
        let joined = caps.name("q").is_none() || !tail.is_empty();
        let value = if joined {
            caps[0][key.len()..].to_owned()
        } else {
            caps.name("quoted").map_or("", |m| m.as_str()).to_owned()
        };
        if !cookie_value_is_credential(&value, key.contains('_')) {
            return caps[0].to_owned();
        }
        let quote = if joined {
            ""
        } else {
            caps.name("q").map_or("", |m| m.as_str())
        };
        format!("{key}{quote}{}{quote}", mask_token(&value))
    })
}
