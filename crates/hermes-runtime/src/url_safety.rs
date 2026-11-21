//! SSRF guard for URLs the agent is asked to fetch or open.
//!
//! Mirrors `tools/url_safety.py`: the host is resolved and every answer must be a public
//! address. DNS failures and ambiguous authorities fail closed. Cloud metadata endpoints stay
//! blocked even when private addresses are allowed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const BLOCKED_HOSTNAMES: [&str; 2] = ["metadata.google.internal", "metadata.goog"];
const METADATA_V4: [Ipv4Addr; 5] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(169, 254, 170, 2),
    Ipv4Addr::new(169, 254, 169, 253),
    Ipv4Addr::new(100, 100, 100, 200),
    Ipv4Addr::new(168, 63, 129, 16),
];

/// An IPv4 address wrapped in IPv6 (`::ffff:a.b.c.d` or `::ffff:0:a.b.c.d`) is judged as that
/// IPv4 address.
fn embedded_ipv4(ip: IpAddr) -> IpAddr {
    if let IpAddr::V6(v6) = ip {
        if let Some(v4) = v6.to_ipv4_mapped() {
            return IpAddr::V4(v4);
        }
        let segments = v6.segments();
        if segments[..4] == [0; 4] && segments[4] == 0xffff && segments[5] == 0 {
            return IpAddr::V4(Ipv4Addr::new(
                (segments[6] >> 8) as u8,
                segments[6] as u8,
                (segments[7] >> 8) as u8,
                segments[7] as u8,
            ));
        }
    }
    ip
}

/// Cloud metadata and the whole link-local range: never a legitimate agent target.
pub(crate) fn is_always_blocked_ip(ip: IpAddr) -> bool {
    match embedded_ipv4(ip) {
        IpAddr::V4(v4) => METADATA_V4.contains(&v4) || v4.is_link_local(),
        IpAddr::V6(v6) => v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    }
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match embedded_ipv4(ip) {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || octets[0] == 0
                // 100.64.0.0/10 (CGNAT), 198.18.0.0/15 (benchmarking), 192.0.0.0/24, 240.0.0.0/4
                || (octets[0] == 100 && (64..128).contains(&octets[1]))
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || octets[0] >= 240
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // unique local
                || (segments[0] & 0xffc0) == 0xfe80 // link-local
                || (segments[0] & 0xffc0) == 0xfec0 // deprecated site-local
                || segments[..4] == [0x100, 0, 0, 0] // discard-only
                || (segments[0] == 0x2001 && segments[1] == 0x2 && segments[2] == 0)
                || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x20)
        }
    }
}

/// The reason a resolved address may not be dialed, or `None`.
pub(crate) fn address_block_reason(ip: IpAddr, allow_private: bool) -> Option<&'static str> {
    if is_always_blocked_ip(ip) {
        return Some("cloud metadata address");
    }
    (!allow_private && is_blocked_ip(ip)).then_some("private/internal address")
}

/// The host of an http(s) URL, or `None` when the URL is not one this guard can judge:
/// userinfo, backslashes or percent-encoding in the authority let another client read a
/// different host than this one.
fn plain_host(url: &str) -> Option<(reqwest::Url, String)> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    let after_scheme = url.trim().split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    if authority.contains(['\\', '%', '@']) {
        return None;
    }
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    // `host_str` keeps the brackets of an IPv6 literal.
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    Some((parsed, host))
}

/// Whether `url` targets only public addresses (or private ones when `allow_private`).
pub async fn is_safe_url(url: &str, allow_private: bool) -> bool {
    let Some((parsed, host)) = plain_host(url) else {
        return false;
    };
    if BLOCKED_HOSTNAMES.contains(&host.as_str()) {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return address_block_reason(ip, allow_private).is_none();
    }
    if !allow_private && (host == "localhost" || host.ends_with(".localhost")) {
        return false;
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let Ok(addresses) = tokio::net::lookup_host((host.as_str(), port)).await else {
        return false;
    };
    let mut any = false;
    for address in addresses {
        any = true;
        if address_block_reason(address.ip(), allow_private).is_some() {
            return false;
        }
    }
    any
}

/// Whether `url` targets the always-blocked floor, whatever the privacy setting.
pub async fn is_always_blocked_url(url: &str) -> bool {
    let Some((parsed, host)) = plain_host(url) else {
        // An ambiguous authority may hide a metadata target.
        return reqwest::Url::parse(url.trim())
            .is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
    };
    if BLOCKED_HOSTNAMES.contains(&host.as_str()) {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_always_blocked_ip(ip);
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let resolved = tokio::net::lookup_host((host.as_str(), port)).await;
    resolved.is_ok_and(|mut addresses| addresses.any(|address| is_always_blocked_ip(address.ip())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_private_loopback_metadata_and_ambiguous_urls() {
        for url in [
            "http://127.0.0.1/",
            "http://[::1]:8080/x",
            "http://10.1.2.3/",
            "http://192.168.0.5",
            "http://172.16.0.1",
            "http://100.64.0.1",
            "http://169.254.169.254/latest/meta-data",
            "http://[::ffff:169.254.169.254]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00:ec2::254]/",
            "http://[fc00::1]/",
            "http://localhost:3000",
            "http://metadata.google.internal/",
            "http://user:pass@example.com/",
            "http://example.com\\@127.0.0.1/",
            "http://%31%32%37.0.0.1/",
            "ftp://example.com/",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(!is_safe_url(url, false).await, "{url}");
        }
        assert!(is_safe_url("http://1.1.1.1/", false).await);
        assert!(is_safe_url("https://[2606:4700:4700::1111]/", false).await);
    }

    #[tokio::test]
    async fn private_opt_in_keeps_the_metadata_floor() {
        assert!(is_safe_url("http://127.0.0.1:8080/", true).await);
        assert!(is_safe_url("http://localhost/", true).await);
        assert!(!is_safe_url("http://169.254.169.254/", true).await);
        assert!(!is_safe_url("http://[::ffff:169.254.169.254]/", true).await);
        assert!(is_always_blocked_url("http://169.254.169.254/").await);
        assert!(!is_always_blocked_url("http://127.0.0.1/").await);
    }
}
