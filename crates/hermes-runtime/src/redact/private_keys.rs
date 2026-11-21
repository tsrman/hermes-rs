//! Whole private-key blocks: PEM, base64 PEM, PGP, PuTTY and age.

use std::sync::LazyLock;

use fancy_regex::Regex as Fancy;
use regex::Regex;

const MARKER: &str = "[REDACTED PRIVATE KEY]";

static PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN[A-Z ]*PRIVATE KEY-----[\s\S]*?-----END[A-Z ]*PRIVATE KEY-----")
        .expect("valid pattern")
});
static B64_PEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"LS0tLS1CRUdJTi[A-Za-z0-9+/=]{60,}").expect("valid pattern"));
static PGP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN PGP PRIVATE KEY BLOCK-----[\s\S]*?-----END PGP PRIVATE KEY BLOCK-----")
        .expect("valid pattern")
});
static AGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AGE-SECRET-KEY-1[0-9A-Z]{20,}").expect("valid pattern"));
/// Possessive quantifiers of the reference are atomic groups here.
static PUTTY: LazyLock<Fancy> = LazyLock::new(|| {
    let eol = r"(?:\r?\n|\\r\\n|\\n)";
    let gutter = r"[ \t]*+(?:[0-9]++(?:[|:]|\t)[ \t]*+)?";
    Fancy::new(&format!(
        r"(?<![\w-])(Private-Lines:[ \t]*+[0-9]++[ \t]*+(?P<eol>{eol}))(?:{gutter}[A-Za-z0-9+/=]{{16,}}+[ \t]*+{eol}?)++"
    ))
    .expect("valid pattern")
});

fn base64_decode(blob: &str) -> Option<Vec<u8>> {
    let mut bits = 0_u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in blob.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

/// Base64 of a PEM private-key block.
fn is_b64_pem_key(blob: &str) -> bool {
    base64_decode(blob).is_some_and(|decoded| {
        let head = &decoded[..decoded.len().min(64)];
        head.starts_with(b"-----BEGIN") && head.windows(11).any(|window| window == b"PRIVATE KEY")
    })
}

pub(super) fn redact(text: &str) -> String {
    let mut text = text.to_owned();
    if text.contains("-----BEGIN") && text.contains("-----END") {
        text = PEM.replace_all(&text, MARKER).into_owned();
    }
    if text.contains("LS0tLS1CRUdJTi") {
        text = B64_PEM
            .replace_all(&text, |caps: &regex::Captures<'_>| {
                if is_b64_pem_key(&caps[0]) {
                    MARKER.to_owned()
                } else {
                    caps[0].to_owned()
                }
            })
            .into_owned();
    }
    if text.contains("END PGP PRIVATE KEY BLOCK") {
        text = PGP.replace_all(&text, MARKER).into_owned();
    }
    if text.contains("Private-Lines:") {
        let mut out = String::new();
        let mut position = 0;
        while let Ok(Some(caps)) = PUTTY.captures_from_pos(text.as_str(), position) {
            let whole = caps.get(0).expect("group 0");
            out.push_str(&text[position..whole.start()]);
            out.push_str(&caps[1]);
            out.push_str(MARKER);
            out.push_str(caps.name("eol").map_or("", |m| m.as_str()));
            position = whole.end();
        }
        out.push_str(&text[position..]);
        text = out;
    }
    if text.contains("AGE-SECRET-KEY-1") {
        text = AGE.replace_all(&text, MARKER).into_owned();
    }
    text
}
