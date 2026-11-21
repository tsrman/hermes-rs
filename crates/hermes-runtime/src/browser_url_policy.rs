//! URL screening for raw CDP calls.
//!
//! Mirrors the helpers of `tools/browser_url_policy.py` that `browser_cdp` uses: extracting URL
//! literals from free text (a JS expression, an HTML snippet, a CDP parameter) and judging them
//! against the cloud-metadata floor and the private-address guard.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::url_safety::{is_always_blocked_ip, is_always_blocked_url, is_safe_url};

/// A URL body: ends at a quote, whitespace, `)`, `]` or `<>`, except that one `[...]` segment
/// (a bracketed IPv6 host) is read whole.
const URL_BODY: &str =
    r#"(?=[^\s'"`)\]<>])(?:[^\s'"`)\[\]<>]*\[[^\s'"`)\[\]<>]*\])?[^\s'"`)\]<>]*"#;

static HTTP_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("(?i)https?://{URL_BODY}")).expect("valid regex"));
/// Plus scheme-relative `//host`, `///host` or `\\host`; not `a//b` or `// note`. One scan, so a
/// `//` inside an http(s) literal's path is part of that literal.
static HTTP_OR_SCHEME_RELATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:https?://|(?<![\w:/\\])[/\\]{{2,}}(?=[^/\\\s])){URL_BODY}"
    ))
    .expect("valid regex")
});
static BODY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{URL_BODY}")).expect("valid regex"));
static USERINFO_TAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^[^\s'"`/?#\\]*@"#).expect("valid regex"));
static LITERAL_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:https?:)?[/\\]+").expect("valid regex"));

/// Every `http(s)://` literal in `text`; with `protocol_relative`, each scheme-relative
/// `//host/...` literal too, kept scheme-less. A literal cut inside its authority with an `@`
/// ahead is also returned as the host a browser reads after that `@`
/// (`http://x)@host/` -> `http://host/`), next to the literal as cut.
pub(crate) fn url_literals(text: &str, protocol_relative: bool) -> Vec<String> {
    let scan = if protocol_relative {
        &*HTTP_OR_SCHEME_RELATIVE
    } else {
        &*HTTP_LITERAL
    };
    let mut found: Vec<String> = Vec::new();
    for found_match in scan.find_iter(text).flatten() {
        let literal = found_match.as_str();
        found.push(literal.to_owned());
        let head = LITERAL_HEAD
            .find(literal)
            .ok()
            .flatten()
            .map_or("", |m| m.as_str());
        if literal[head.len()..].contains(['/', '?', '#', '\\']) {
            continue; // the authority ended inside the literal: its host is fixed
        }
        let rest = &text[found_match.end()..];
        let Some(tail) = USERINFO_TAIL.find(rest).ok().flatten() else {
            continue;
        };
        if let Some(host) = BODY.find(&rest[tail.end()..]).ok().flatten() {
            found.push(format!("{head}{}", host.as_str()));
        }
    }
    found
        .into_iter()
        .map(|literal| literal.trim_end_matches(['.', ',', ';']).to_owned())
        .collect()
}

/// Whether `url` hits the always-blocked cloud-metadata floor or fails the SSRF guard.
pub(crate) async fn url_blocked(url: &str) -> bool {
    is_always_blocked_url(url).await || !is_safe_url(url, false).await
}

/// The first `http(s)://` literal in a JS expression that `url_blocked` refuses (best effort).
pub(crate) async fn expression_targets_private_url(expression: &str) -> Option<String> {
    for literal in url_literals(expression, false) {
        if url_blocked(&literal).await {
            return Some(literal);
        }
    }
    None
}

/// The host of `http://<authority>...` as the reference's `urlsplit(...).hostname` reads it.
fn split_hostname(url: &str) -> Result<String, ()> {
    let cleaned: String = url
        .trim_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c'])
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let after = cleaned.split_once("://").map_or("", |(_, rest)| rest);
    let netloc = after.split(['/', '?', '#']).next().unwrap_or("");
    if netloc.contains('[') != netloc.contains(']') {
        return Err(()); // urlsplit: Invalid IPv6 URL
    }
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
    let host = if let Some(inner) = hostinfo.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        hostinfo.split(':').next().unwrap_or("")
    };
    Ok(host.to_owned())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `socket.inet_aton`: one to four numbers (decimal, `0x` hex or leading-zero octal); the last
/// fills the remaining bytes.
fn inet_aton(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut numbers: Vec<u64> = Vec::with_capacity(parts.len());
    for part in &parts {
        let (digits, radix) =
            if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                (if hex.is_empty() { "0" } else { hex }, 16)
            } else if part.len() > 1 && part.starts_with('0') {
                (&part[1..], 8)
            } else {
                (*part, 10)
            };
        numbers.push(u64::from_str_radix(digits, radix).ok()?);
    }
    let (last, leading) = numbers.split_last()?;
    if leading.iter().any(|n| *n > 255) {
        return None;
    }
    let remaining_bits = 8 * (4 - leading.len() as u32);
    if remaining_bits < 32 && *last >= 1u64 << remaining_bits {
        return None;
    }
    if *last > u64::from(u32::MAX) {
        return None;
    }
    let mut value = *last;
    for (index, number) in leading.iter().enumerate() {
        value |= number << (8 * (3 - index as u32));
    }
    Some(Ipv4Addr::from(value as u32))
}

/// Whether a URL literal hits the cloud-metadata floor. A scheme-relative one (`//host`,
/// `\\host`) is judged by its literal host only (a metadata hostname or IP, decimal/hex/octal
/// IPv4 spellings included), never resolved, so a `//TODO` comment costs no DNS lookup.
pub(crate) async fn literal_targets_metadata(literal: &str) -> bool {
    if !literal.starts_with(['/', '\\']) {
        return is_always_blocked_url(literal).await;
    }
    let url = format!(
        "http://{}",
        literal.trim_start_matches(['/', '\\']).replace('\\', "/")
    );
    // A control character can move the host, as for http(s).
    if url.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return true;
    }
    let Ok(host) = split_hostname(&url) else {
        return false;
    };
    let after = url.split_once("://").map_or("", |(_, rest)| rest);
    if after
        .split(['/', '?', '#'])
        .next()
        .is_none_or(str::is_empty)
    {
        return true; // authority not introduced by //
    }
    let host: String = percent_decode(&host)
        .chars()
        .map(|c| {
            if matches!(c, '。' | '．' | '｡') {
                '.'
            } else {
                c
            }
        })
        .collect();
    let host = host.trim().to_lowercase();
    let host = host.trim_end_matches('.');
    if matches!(host, "metadata.google.internal" | "metadata.goog") {
        return true;
    }
    let ip = host.parse::<IpAddr>().ok().or_else(|| {
        (!host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f' | 'x' | '.')))
        .then(|| inet_aton(host))
        .flatten()
        .map(IpAddr::V4)
    });
    ip.is_some_and(is_always_blocked_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_literals_and_scheme_relative_hosts() {
        let found = url_literals("fetch('http://10.0.0.1/a?b=//c') + '//evil.test/x'", true);
        assert_eq!(found, ["http://10.0.0.1/a?b=//c", "//evil.test/x"]);
        assert!(url_literals("a//b // note", true).is_empty());
        assert_eq!(
            url_literals("go http://x)@10.0.0.2/", false),
            ["http://x", "http://10.0.0.2/"]
        );
    }

    #[test]
    fn inet_aton_reads_every_ipv4_spelling() {
        let target = Ipv4Addr::new(169, 254, 169, 254);
        assert_eq!(inet_aton("0xa9fea9fe"), Some(target));
        assert_eq!(inet_aton("2852039166"), Some(target));
        assert_eq!(inet_aton("0251.0376.0251.0376"), Some(target));
        assert_eq!(inet_aton("169.16689662"), Some(target));
        assert_eq!(inet_aton("256.1.1.1"), None);
    }
}
