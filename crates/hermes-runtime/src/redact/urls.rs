//! Credentials in URLs: database URL query passwords, bare tokens in user information, and
//! `user:password@` pairs. Web-URL query strings are passed through on purpose, so a magic
//! link or an OAuth callback keeps working.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

use super::mask::mask_token;
use super::tokens::replace_all;

static DSN_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:<(?:[a-z][a-z_ -]*|[A-Z][A-Z_ -]*)>|\$\{?[A-Z_][A-Z0-9_]*\}?|%[A-Z_][A-Z0-9_]*%)$",
    )
    .expect("valid pattern")
});
const PLACEHOLDER_WORDS: [&str; 7] = ["password", "passwd", "pass", "pw", "pwd", "secret", "token"];

/// A template (`{pw}`), a docs placeholder or a field name, never a literal secret.
pub(super) fn is_userinfo_placeholder(password: &str) -> bool {
    (password.starts_with('{') && password.ends_with('}'))
        || DSN_PLACEHOLDER.is_match(password)
        || PLACEHOLDER_WORDS.contains(&password.to_lowercase().as_str())
}

/// A raw `{` is not legal in a URL query, so a value that opens one is a template.
fn is_template_value(value: &str) -> bool {
    value.starts_with(['{']) || value.starts_with("${") || is_userinfo_placeholder(value)
}

/// DB URLs also take the password as a query parameter, never as a round-trip token.
static DB_URL_QUERY: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(
        r#"(?i)(?:postgres(?:ql)?|mysql|mariadb|mongodb|rediss?|amqps?)(?:\+(?>[a-z0-9]+))?://(?>(?:[^\s?#:]|:(?!//))*)\?[^\s#"<>]+"#,
    )
    .expect("valid pattern")
});
static QUERY_PW_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[?&;](?:(?:ssl)?password|passwd|pwd)=").expect("valid pattern")
});
static QUERY_PW_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^[^;&#\s"'<>]*"#).expect("valid pattern"));
static QUERY_PW_REST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^[^&#\s"'<>]*"#).expect("valid pattern"));
static SIBLING_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^;[A-Za-z_][A-Za-z0-9_.-]*=").expect("valid pattern"));

fn mask_query_params(query: &str) -> String {
    let mut out = String::new();
    let mut position = 0;
    while let Some(key) = QUERY_PW_KEY.find_at(query, position) {
        let head_end = key.end()
            + QUERY_PW_HEAD
                .find(&query[key.end()..])
                .map_or(0, |m| m.end());
        let head = &query[key.end()..head_end];
        out.push_str(&query[position..key.end()]);
        if is_template_value(head) && SIBLING_KEY.is_match(&query[head_end..]) {
            // The sibling is judged on its own, so it does not ride the template exemption.
            out.push_str(head);
            position = head_end;
            continue;
        }
        position = head_end
            + QUERY_PW_REST
                .find(&query[head_end..])
                .map_or(0, |m| m.end());
        // A template is kept only as the whole value.
        if position == head_end && is_template_value(head) {
            out.push_str(head);
        } else if position > key.end() {
            out.push_str("***");
        }
    }
    out.push_str(&query[position..]);
    out
}

pub(super) fn redact_db_url_query_passwords(text: &str) -> String {
    replace_all(&DB_URL_QUERY, text, |caps| mask_query_params(&caps[0]))
}

/// `https://TOKEN@host`: a single opaque credential in the user-information position.
static URL_BARE_TOKEN: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r#"(?i)((?:https?|wss?|git|ssh|ftp|ftps|sftp)://)([^\s:@/"]{8,})(@[^\s]+)"#)
        .expect("valid pattern")
});

pub(super) fn redact_url_bare_tokens(text: &str) -> String {
    replace_all(&URL_BARE_TOKEN, text, |caps| {
        format!("{}{}{}", &caps[1], mask_token(&caps[2]), &caps[3])
    })
}

static URL_USERINFO_PAIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(//)([^/\s?#@"]+)@"#).expect("valid pattern"));

/// The password half of `scheme://user:pass@host`; scheme, user and host stay visible.
pub(super) fn redact_userinfo_passwords(text: &str) -> String {
    URL_USERINFO_PAIR
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let userinfo = &caps[2];
            let Some((user, password)) = userinfo.split_once(':') else {
                return caps[0].to_owned();
            };
            if password.is_empty() || is_userinfo_placeholder(password) {
                return caps[0].to_owned();
            }
            format!("{}{user}:***@", &caps[1])
        })
        .into_owned()
}

/// A scheme-less network path (`mount //user:pass@host/share`), masked only on a safety
/// boundary: the password needs 8 or more characters.
static NETWORK_PATH_USERINFO: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r#"(?<![^\s"'=(`,\[])(//)([^/\s?#@:"]+):([^/\s?#@"]{8,})@"#).expect("valid pattern")
});

pub(super) fn redact_network_path_userinfo(text: &str) -> String {
    if !text.contains("//") {
        return text.to_owned();
    }
    replace_all(&NETWORK_PATH_USERINFO, text, |caps| {
        if is_userinfo_placeholder(&caps[3]) {
            return caps[0].to_owned();
        }
        format!("{}{}:***@", &caps[1], &caps[2])
    })
}

const SENSITIVE_QUERY_PARAMS: [&str; 19] = [
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "api_key",
    "apikey",
    "client_secret",
    "password",
    "auth",
    "jwt",
    "session",
    "secret",
    "key",
    "code",
    "signature",
    "x-amz-signature",
    "x-goog-signature",
    "sig",
    "usertoken",
];

static FORM_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_.-]*=[^&\s]*(?:&[A-Za-z_][A-Za-z0-9_.-]*=[^&\s]*)+$")
        .expect("valid pattern")
});

/// Values of sensitive `k=v&k=v` parameters; the other pairs pass through.
fn redact_query_string(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if SENSITIVE_QUERY_PARAMS.contains(&key.to_lowercase().as_str()) => {
                format!("{key}=***")
            }
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Sensitive values when the whole text is a clean `k=v&k=v` body.
pub(super) fn redact_form_body(text: &str) -> String {
    if text.is_empty()
        || text.contains('\n')
        || !text.contains('&')
        || !FORM_BODY.is_match(text.trim())
    {
        return text.to_owned();
    }
    redact_query_string(text.trim())
}
