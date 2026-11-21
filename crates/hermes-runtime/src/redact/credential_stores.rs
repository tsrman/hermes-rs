//! Credential-store lines: `.netrc`, `.pgpass`, `.npmrc` and INI stores (`.pypirc`,
//! `~/.aws/credentials`), which have no `KEY=value` shape the assignment passes understand.
//! They run only for a source already classified as secret-bearing.

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex as Fancy};
use regex::Regex;

use super::assignments::should_redact_assignment;
use super::mask::mask_token;
use super::tokens::replace_all;

const GUTTER: &str = r"(?:[0-9]+(?:[|:\-]|\t)[ \t]*)?";
const SECRET_CFG_NAMES: &str = r"(?:api[ _.\-]?key|token|secret|passwd|password|credential|auth)";

static INI_SECTION: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?m)^[ \t]*{GUTTER}\[[^\[\]\s][^\[\]\r\n]*\](?>[ \t]*)$"
    ))
    .expect("valid pattern")
});
static AWS_PROCESS_KEY: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?m)^[ \t]*{GUTTER}credential_process(?>[ \t]*)=(?>[ \t]*)"
    ))
    .expect("valid pattern")
});

/// Byte spans of the `credential_process =` keys that follow an INI section header. The key
/// names a helper command, not the secret, so the assignment passes leave it alone.
pub(super) fn aws_credential_process_keys(text: &str) -> Vec<(usize, usize)> {
    if !text.contains("credential_process") {
        return Vec::new();
    }
    let Ok(Some(header)) = INI_SECTION.find(text) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut from = header.end();
    while let Ok(Some(found)) = AWS_PROCESS_KEY.find_from_pos(text, from) {
        spans.push((found.start(), found.end()));
        from = found.end().max(found.start() + 1);
    }
    spans
}

/// Apply `body` to the text between `spans`; the spans stay as they are. A segment that follows
/// a span is mid-line, so it is passed behind a NUL that `^`-anchored passes cannot take for a
/// line start.
pub(super) fn outside_spans(
    text: &str,
    spans: &[(usize, usize)],
    body: impl Fn(&str) -> String,
) -> String {
    let mut out = String::new();
    let mut position = 0;
    for &(start, end) in spans {
        let segment = &text[position..start];
        if position > 0 {
            let done = body(&format!("\0{segment}"));
            out.push_str(done.strip_prefix('\0').unwrap_or(&done));
        } else {
            out.push_str(&body(segment));
        }
        out.push_str(&text[start..end]);
        position = end;
    }
    if position > 0 {
        let done = body(&format!("\0{}", &text[position..]));
        out.push_str(done.strip_prefix('\0').unwrap_or(&done));
    } else {
        out.push_str(&body(text));
    }
    out
}

static NETRC_LINE: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?m)^([ \t]*{GUTTER})((?:machine|default|login|account|port|password)\b[^\r\n]*)"
    ))
    .expect("valid pattern")
});
static NETRC_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"\r\n]*"?|\S+"#).expect("valid pattern"));
const NETRC_KEYWORDS: [&str; 5] = ["machine", "login", "password", "account", "port"];

static STORE_PATTERNS: LazyLock<Vec<Fancy>> = LazyLock::new(|| {
    vec![
        // `.pgpass`: host and port never hold `=` or `#`; a database or user name may.
        Fancy::new(&format!(
            r"(?m)(^[ \t]*{GUTTER}(?:[^:\s=#]*:){{2}}(?:[^:\s]*:){{2}})(\S[^\r\n]*)"
        ))
        .expect("valid pattern"),
        // `.npmrc`: a value that is only a `${NAME}` reference names a variable, not a secret.
        Fancy::new(
            r#"(?m)(:_(?:authToken|auth|password)[ \t]*=[ \t]*)(?!"?\$\{\w+\??\}"?[ \t\r]*$)(\S(?:[^\r\n]*\S)?)"#,
        )
        .expect("valid pattern"),
    ]
});

fn ini_pattern(delimiter: &str) -> Fancy {
    Fancy::new(&format!(
        r"(?mi)(^[ \t]*{GUTTER}((?>[A-Za-z0-9_.\-]*)(?>(?:(?<=api) (?=key)(?>[A-Za-z0-9_.\-]*))?))(?:(?>[ \t]+){d}(?>[ \t]*)|{d}(?>[ \t]+)|{d}(?=\S+[ \t]+[^\s#])))(\S(?:[^\r\n]*\S)?)",
        d = delimiter
    ))
    .expect("valid pattern")
}

static INI_SPACED: LazyLock<Fancy> = LazyLock::new(|| ini_pattern("="));
static INI_COLON: LazyLock<Fancy> = LazyLock::new(|| ini_pattern("[=:]"));
static SECRET_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("(?i){SECRET_CFG_NAMES}")).expect("valid pattern"));

fn already_masked(value: &str) -> bool {
    value == "***" || value.starts_with("«redacted")
}

fn netrc_line(caps: &Captures<'_, str>) -> String {
    let line = &caps[2];
    let mut out = String::new();
    let mut position = 0;
    let mut keyword: Option<&str> = None;
    let mut previous: Option<&str> = None;
    for token in NETRC_TOKEN.find_iter(line) {
        let value = token.as_str();
        // The token after a keyword is its value; a non-keyword after any `password` token is
        // masked too, since `login password X` is ambiguous.
        let secret = keyword == Some("password")
            || (previous == Some("password") && !NETRC_KEYWORDS.contains(&value));
        // A bare `=` or `:` is an INI `password = x` line, left to the INI pass.
        let bare = value.trim_start_matches(['=', ':']);
        if secret && !bare.is_empty() && !already_masked(bare) {
            out.push_str(&line[position..token.start()]);
            out.push_str(&mask_token(value));
            position = token.end();
        }
        keyword = if keyword.is_some() {
            None
        } else if NETRC_KEYWORDS.contains(&value) {
            Some(value)
        } else {
            None
        };
        previous = Some(value);
    }
    format!("{}{}{}", &caps[1], out, &line[position..])
}

/// Mask the value of every credential-store line: in a credential store the value is a secret
/// whatever it looks like, so a weak human password is masked too.
pub(super) fn redact_credential_store_lines(text: &str) -> String {
    // INI first: the netrc scan would mask only the first word of `password =a b`.
    let ini = if INI_SECTION.is_match(text).unwrap_or(false) {
        &*INI_COLON
    } else {
        &*INI_SPACED
    };
    let mut text = outside_spans(text, &aws_credential_process_keys(text), |segment| {
        replace_all(ini, segment, |caps| {
            let (key, value) = (&caps[1], &caps[3]);
            if !SECRET_WORD.is_match(&caps[2]) || !should_redact_assignment(key, value, true) {
                return caps[0].to_owned();
            }
            format!("{key}{}", mask_token(value))
        })
    });
    text = replace_all(&NETRC_LINE, &text, netrc_line);
    for pattern in STORE_PATTERNS.iter() {
        text = replace_all(pattern, &text, |caps| {
            if already_masked(&caps[2]) {
                return caps[0].to_owned();
            }
            format!("{}{}", &caps[1], mask_token(&caps[2]))
        });
    }
    text
}
