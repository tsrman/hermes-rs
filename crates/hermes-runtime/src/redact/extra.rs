//! Smaller passes: Zhipu and Discord tokens, partial private-key prints, and `NAME=user:pass`
//! pairs under a name that says they hold one.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

use super::mask::mask_token;
use super::tokens::replace_all;
use super::urls::is_userinfo_placeholder;

/// Zhipu keys are `id.secret`: a 32-digit lowercase hex id and a run of 16 or more letters
/// and digits, so content-hash file names never match.
static ZHIPU: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r"(?<![A-Za-z0-9_.-])([0-9a-f]{32}\.[A-Za-z0-9]{16,})(?![A-Za-z0-9_.-])")
        .expect("valid pattern")
});

pub(super) fn redact_zhipu(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    replace_all(&ZHIPU, text, |caps| mask_token(&caps[1]))
}

const TAIL: &str = r"(?![A-Za-z0-9_-]|\.[A-Za-z0-9_-])";
/// Candidates are found by lookahead, so overlapping ones are all seen; the decode below
/// rejects every start that is not a real user id.
static DISCORD: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(&format!(
        r"(?=(mfa\.[A-Za-z0-9_-]{{84}}){TAIL})|(?=([MNO][A-Za-z0-9_-]{{22,27}}\.[A-Za-z0-9_-]{{6,7}}\.[A-Za-z0-9_-]{{27,}}){TAIL})"
    ))
    .expect("valid pattern")
});
static ESCAPE_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\\[nrtbf]|\\x[0-9A-Fa-f]{2}|%[0-9A-Fa-f]{2}|\\u[0-9A-Fa-f]{4}|(?:\x1b|\\x1b|\\033|\\e)\[[0-9;]*m)$")
        .expect("valid pattern")
});

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// `mfa.` has no id to decode, so it needs a word boundary or an escape spelling on its left.
fn mfa_left_ok(before: &str) -> bool {
    match before.chars().next_back() {
        None => true,
        Some(c) if !is_token_char(c) => true,
        Some(_) => {
            let window: String = before
                .chars()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            ESCAPE_TAIL.is_match(&window)
        }
    }
}

/// The first segment must base64-decode to a snowflake of 17 to 20 digits.
fn is_discord_token(candidate: &str) -> bool {
    let head = candidate.split('.').next().unwrap_or("");
    let mut bits = 0_u32;
    let mut count = 0;
    let mut decoded = Vec::new();
    for c in head.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return false,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            decoded.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    (17..=20).contains(&decoded.len()) && decoded.iter().all(u8::is_ascii_digit)
}

pub(super) fn redact_discord_tokens(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut position = 0;
    let mut from = 0;
    while from <= text.len() {
        let Ok(Some(caps)) = DISCORD.captures_from_pos(text, from) else {
            break;
        };
        let whole = caps.get(0).expect("group 0");
        from = whole.end() + text[whole.end()..].chars().next().map_or(1, char::len_utf8);
        let (token, mfa) = match (caps.get(1), caps.get(2)) {
            (Some(token), _) => (token, true),
            (_, Some(token)) => (token, false),
            _ => continue,
        };
        let start = token.start();
        if start < position
            || !(if mfa {
                mfa_left_ok(&text[..start])
            } else {
                is_discord_token(token.as_str())
            })
        {
            continue;
        }
        out.push_str(&text[position..start]);
        out.push_str(&mask_token(token.as_str()));
        position = token.end();
    }
    out.push_str(&text[position..]);
    out
}

static ARMOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----(BEGIN|END)[A-Z ]*PRIVATE KEY-----").expect("valid pattern")
});
/// A pure-hex line is a digest in documentation, not key material.
static BODY_LINE: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(r"^[ \t]*(?![0-9A-Fa-f]+[ \t]*$)[A-Za-z0-9+/=]{16,}[ \t]*$").expect("valid pattern")
});
static TRAILER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*(?:[A-Za-z0-9+/]{1,15}={0,2}|==?)[ \t]*$").expect("valid pattern")
});

fn is_body_line(line: &str) -> bool {
    BODY_LINE
        .is_match(line.trim_end_matches('\r'))
        .unwrap_or(false)
}

/// The base64 body run next to a lone BEGIN (forward) or END (backward) armor line, as left by
/// `head` or `tail` of a key file.
pub(super) fn redact_partial_private_keys(text: &str) -> String {
    if !text.contains("PRIVATE KEY-----") {
        return text.to_owned();
    }
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let mut drop = vec![false; lines.len()];
    for i in 0..lines.len() {
        let Some(armor) = ARMOR.captures(&lines[i]) else {
            continue;
        };
        let forward = &armor[1] == "BEGIN";
        let mut j = i as isize + if forward { 1 } else { -1 };
        let step: isize = if forward { 1 } else { -1 };
        let at = |k: isize| -> Option<usize> {
            (k >= 0 && (k as usize) < lines.len()).then_some(k as usize)
        };
        if !forward && j > 0 {
            if let (Some(jj), Some(prev)) = (at(j), at(j - 1)) {
                if TRAILER.is_match(lines[jj].trim_end_matches('\r')) && is_body_line(&lines[prev])
                {
                    drop[jj] = true;
                    j -= 1;
                }
            }
        }
        while let Some(jj) = at(j) {
            if drop[jj] || !is_body_line(&lines[jj]) {
                break;
            }
            drop[jj] = true;
            j += step;
        }
        if forward && (i as isize + 1) < j {
            if let Some(jj) = at(j) {
                if TRAILER.is_match(lines[jj].trim_end_matches('\r'))
                    && lines[jj].trim_end().ends_with('=')
                {
                    drop[jj] = true;
                }
            }
        }
        if j != i as isize + step {
            lines[i] = "[REDACTED PRIVATE KEY]".into();
        }
    }
    if !drop.iter().any(|d| *d) {
        return text.to_owned();
    }
    lines
        .into_iter()
        .zip(drop)
        .filter(|(_, dropped)| !dropped)
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

static BASIC_PAIR_ASSIGN: LazyLock<Fancy> = LazyLock::new(|| {
    Fancy::new(
        r#"(?<![A-Za-z0-9_.\-])([A-Za-z][A-Za-z0-9_.\-]{0,63})=(["']?)([A-Za-z0-9._~\-]{2,})(?::(?!//)([^\s'"/:@]+))\2(?![^\s;,&|)\]}"'`])"#,
    )
    .expect("valid pattern")
});
static BASIC_PAIR_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)auth|basic|cred|login|proxy|userpass|userinfo").expect("valid pattern")
});
static VERSION_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[vV]?\d+(\.\d+)+").expect("valid pattern"));
static WORD_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]+(?:[.\-][a-z]+)*$").expect("valid pattern"));
const HASH_ALGO_NAMES: [&str; 15] = [
    "md5",
    "sha1",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "sha3-224",
    "sha3-256",
    "sha3-384",
    "sha3-512",
    "blake2b",
    "blake2s",
    "ripemd160",
    "crc32",
    "crc32c",
];

/// 8 or more characters with a letter, and not a version-shaped or word-shaped tail.
fn is_basic_pair_password(password: &str) -> bool {
    if VERSION_TAG.is_match(password) || WORD_TAG.is_match(password) {
        return false;
    }
    password.chars().count() >= 8 && password.chars().any(char::is_alphabetic)
}

/// `NAME=user:pass` under a name that says it holds a Basic pair (`IMAGE=redis:7-alpine` is not one).
pub(super) fn redact_basic_pairs(text: &str) -> String {
    if !text.contains('=') {
        return text.to_owned();
    }
    replace_all(&BASIC_PAIR_ASSIGN, text, |caps| {
        let Some(password) = caps.get(4).map(|m| m.as_str()) else {
            return caps[0].to_owned();
        };
        if !BASIC_PAIR_NAME.is_match(&caps[1]) || !is_basic_pair_password(password) {
            return caps[0].to_owned();
        }
        // `CHECKSUM=sha256:<hex>` is an algorithm and a digest.
        if HASH_ALGO_NAMES.contains(&caps[3].to_lowercase().as_str())
            || is_userinfo_placeholder(password)
        {
            return caps[0].to_owned();
        }
        format!("{}={}{}:***{}", &caps[1], &caps[2], &caps[3], &caps[2])
    })
}
