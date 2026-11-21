//! Credentials with a fixed shape of their own: authorization headers, Telegram bot tokens,
//! passwords in database connection strings, and JWTs.

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex as Fancy};
use regex::Regex;

use super::mask::mask_token;

const SCHEME_WORD: &str =
    r#"(?:bearer|basic|bot|token|digest|negotiate|ntlm|oauth)(?=[`)\],.;:!?]*(?:[\s"']|$))"#;

/// Authorization and Proxy-Authorization headers. The header name and the scheme word stay.
static AUTH_HEADER: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r#"(?i)((?:Proxy-)?Authorization:\s*)([A-Za-z][\w.+-]*\s+)?(?!{SCHEME_WORD})([^\s"']+)"#
    ))
    .expect("valid pattern")
});
static AUTH_GATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)uthorization").expect("valid pattern"));

pub(super) fn redact_auth_headers(text: &str) -> String {
    if !AUTH_GATE.is_match(text) {
        return text.to_owned();
    }
    replace_all(&AUTH_HEADER, text, |caps| {
        format!(
            "{}{}{}",
            &caps[1],
            caps.get(2).map_or("", |m| m.as_str()),
            mask_token(&caps[3])
        )
    })
}

/// Telegram bot tokens: `[bot]<digits>:<token>` with a token of 30 or more characters.
static TELEGRAM: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(concat!(
        r"(?P<boundary>\\[nrtbfv]|\\x[0-9a-fA-F]{2}|\\u[0-9a-fA-F]{4}|",
        r"\\U[0-9a-fA-F]{8}|%[0-9a-fA-F]{2}|(?P<csi>(?:\x1b|\\x1b|\\u001[bB]|\\033)\[[0-?]*[ -/]*[@-~])|(?<![A-Za-z_\d]))",
        r"(?P<bot>bot)?(?P<id>\d{8,}):(?P<body>[-A-Za-z0-9_]{30,})"
    ))
    .expect("valid pattern")
});

pub(super) fn redact_telegram(text: &str) -> String {
    if !text.contains(':') {
        return text.to_owned();
    }
    replace_all(&TELEGRAM, text, |caps| {
        let boundary = caps.name("boundary").map_or("", |m| m.as_str());
        // An encoded identifier character is still an identifier, not a separator.
        if boundary.chars().count() > 2 && caps.name("csi").is_none() {
            let digits = boundary.strip_prefix('%').unwrap_or(&boundary[2..]);
            let digits = if boundary.starts_with('%') {
                digits
            } else {
                &boundary[2..]
            };
            if let Some(character) = u32::from_str_radix(digits, 16)
                .ok()
                .and_then(char::from_u32)
            {
                if character.is_ascii_alphabetic() || character == '_' || character.is_numeric() {
                    return caps[0].to_owned();
                }
            } else {
                return caps[0].to_owned();
            }
        }
        format!(
            "{boundary}{}{}:***",
            caps.name("bot").map_or("", |m| m.as_str()),
            &caps["id"]
        )
    })
}

/// `scheme://user:PASSWORD@host` for the database schemes. The password group forbids
/// whitespace, so a match never spans a line break.
static DB_CONNSTR: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(
        r"(?i)((?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|rediss?|amqps?)://(?>[^:\s]*):)((?>(?:[^@\s:]+|:(?!//))+))(@)",
    )
    .expect("valid pattern")
});
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:<(?:[a-z][a-z_ -]*|[A-Z][A-Z_ -]*)>|\$\{?[A-Z_][A-Z0-9_]*\}?|%[A-Z_][A-Z0-9_]*%)$",
    )
    .expect("valid pattern")
});

pub(super) fn redact_db_urls(text: &str, code_file: bool) -> String {
    if !text.contains("://") {
        return text.to_owned();
    }
    replace_all(&DB_CONNSTR, text, |caps| {
        // In source, a pure `{...}` password is an f-string template reference.
        if code_file && caps[2].starts_with('{') && caps[2].ends_with('}') {
            return caps[0].to_owned();
        }
        // A password-only URL in documentation names a placeholder, which is not a credential.
        if caps[1].ends_with("://:") && PLACEHOLDER.is_match(&caps[2]) {
            return caps[0].to_owned();
        }
        format!("{}***{}", &caps[1], &caps[3])
    })
}

const WEBHOOK_HOST_END: &str = r"\.?(?::\d+)?/";

/// Incoming-webhook URLs: the URL is the credential, so only its secret segment is masked.
static WEBHOOK: LazyLock<Fancy> = LazyLock::new(|| {
    let end = WEBHOOK_HOST_END;
    Fancy::new(&format!(
        concat!(
            r"(?i)(https?://hooks\.slack(?:-gov)?\.com{end}workflows/[A-Z0-9]+/[A-Z0-9]+/\d+/)([A-Za-z0-9]{{8,}})",
            r"|(https?://hooks\.slack(?:-gov)?\.com{end}(?:services|workflows|triggers)/[A-Z0-9]+/[A-Z0-9]+/)(?!\d+/)([A-Za-z0-9]{{8,}})",
            r"|(https?://(?:canary\.|ptb\.|www\.)?discord(?:app)?\.com{end}api/(?:v\d+/)?webhooks/\d+/)([\w-]{{8,}})",
            r"|(https?://[A-Za-z0-9.-]*\.?(?:webhook\.office\.com{end}webhookb2|outlook\.office\.com{end}webhook)/)([\w@/%+=-]{{8,}})",
            r"|(https?://[A-Za-z0-9.-]+\.(?:logic\.azure\.com|api\.powerplatform\.com){end}",
            r#"(?:[^\s?#\"'<>/]{{1,128}}/){{0,8}}workflows/[^\s?#\"'<>]{{1,512}}?\?"#,
            r#"(?:[^\s#\"'<>]{{0,1024}}?&)?sig=)([A-Za-z0-9_%-]{{8,}})"#
        ),
        end = end
    ))
    .expect("valid pattern")
});

pub(super) fn redact_webhooks(text: &str) -> String {
    if !text.contains("://") {
        return text.to_owned();
    }
    replace_all(&WEBHOOK, text, |caps| {
        for pair in 0..5 {
            if let (Some(prefix), Some(secret)) = (caps.get(1 + pair * 2), caps.get(2 + pair * 2)) {
                if !prefix.as_str().is_empty() {
                    return format!("{}{}", prefix.as_str(), mask_token(secret.as_str()));
                }
            }
        }
        caps[0].to_owned()
    })
}

/// Links that sign a browser in as the user: the session credential and the one-use ticket.
static WEBAPP_LINK: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r"((?<![A-Za-z0-9_-])hermes-session=|/webapp/launch#(?:[^\s#]*?&)?ticket=)[A-Za-z0-9_-]{20,}")
        .expect("valid pattern")
});

pub(super) fn redact_webapp_links(text: &str) -> String {
    if !text.contains("hermes-session=") && !text.contains("/webapp/launch#") {
        return text.to_owned();
    }
    replace_all(&WEBAPP_LINK, text, |caps| format!("{}***", &caps[1]))
}

/// E.164 numbers of 7 to 15 digits. A `+` glued to a word or a dot is a version suffix.
static PHONE: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r"(?<![0-9A-Za-z_.])(\+[1-9][0-9]{6,14})(?![A-Za-z0-9])").expect("valid pattern")
});

pub(super) fn redact_phone_numbers(text: &str) -> String {
    if !text.contains('+') {
        return text.to_owned();
    }
    replace_all(&PHONE, text, |caps| {
        let phone: Vec<char> = caps[1].chars().collect();
        let keep = if phone.len() <= 8 { 2 } else { 4 };
        let head: String = phone[..keep].iter().collect();
        let tail: String = phone[phone.len() - keep..].iter().collect();
        format!("{head}****{tail}")
    })
}

static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_=-]{4,}){0,2}").expect("valid pattern")
});

pub(super) fn redact_jwt(text: &str) -> String {
    if !text.contains("eyJ") {
        return text.to_owned();
    }
    JWT.replace_all(text, |caps: &regex::Captures<'_>| mask_token(&caps[0]))
        .into_owned()
}

/// `Regex::replace_all` for the lookaround engine.
pub(super) fn replace_all(
    pattern: &Fancy,
    text: &str,
    replacement: impl Fn(&Captures<'_, str>) -> String,
) -> String {
    let mut out = String::new();
    let mut position = 0;
    let mut search_from = 0;
    while search_from <= text.len() {
        let Ok(Some(caps)) = pattern.captures_from_pos(text, search_from) else {
            break;
        };
        let whole = caps.get(0).expect("group 0");
        out.push_str(&text[position..whole.start()]);
        out.push_str(&replacement(&caps));
        position = whole.end();
        search_from = if whole.end() > whole.start() {
            whole.end()
        } else {
            out.push_str(
                &text[whole.end()..]
                    .chars()
                    .next()
                    .map(String::from)
                    .unwrap_or_default(),
            );
            position = whole.end() + text[whole.end()..].chars().next().map_or(0, char::len_utf8);
            position.max(whole.end() + 1)
        };
    }
    out.push_str(&text[position.min(text.len())..]);
    out
}
