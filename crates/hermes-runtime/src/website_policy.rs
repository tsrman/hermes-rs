//! Website access policy (`security.website_blocklist` in `config.yaml` plus optional
//! `shared_files`), ported from `tools/website_policy.py` and verified against
//! `crates/hermes-runtime/tests/fixtures/web_remainder_cases.json`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct WebsiteRule {
    pub pattern: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct WebsitePolicy {
    pub enabled: bool,
    pub rules: Vec<WebsiteRule>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WebsiteBlock {
    pub url: String,
    pub host: String,
    pub rule: String,
    pub source: String,
    pub message: String,
}

impl WebsiteBlock {
    pub fn policy_metadata(&self) -> Value {
        json!({
            "host": self.host,
            "rule": self.rule,
            "source": self.source,
        })
    }
}

/// Reduce a rule (bare host, URL, or `host/path`) to a lowercase host; `None` for blanks/comments.
/// Mirrors `tools/website_policy.py::_normalize_rule`.
pub fn normalize_rule(rule: &str) -> Option<String> {
    let mut value = rule.trim().to_ascii_lowercase();
    if value.is_empty() || value.starts_with('#') {
        return None;
    }
    if let Some((_, after)) = value.split_once("://") {
        // Python `urlparse`: `netloc` is everything up to `/`, `?`, `#`; if empty, `path` is used.
        let netloc = after.split(['/', '?', '#']).next().unwrap_or("");
        if !netloc.is_empty() {
            value = netloc.to_owned();
        } else {
            let path = after.split(['?', '#']).next().unwrap_or("");
            value = path.to_owned();
        }
    }
    let head = value.split('/').next().unwrap_or("").trim();
    let trimmed = head.trim_end_matches('.');
    let stripped = trimmed.strip_prefix("www.").unwrap_or(trimmed);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

/// Normalize a host string (`tools/url_safety.py::_normalize_hostname`).
fn normalize_hostname(raw: &str) -> String {
    let mut host = raw.trim().to_ascii_lowercase();
    if let Some(inner) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        host = inner.to_owned();
    }
    host.trim_end_matches('.').to_owned()
}

/// Extract normalized hostname from a URL or schemeless URLish string.
/// Mirrors `tools/website_policy.py::_extract_host_from_urlish`.
pub fn extract_host_from_urlish(url: &str) -> String {
    let host = parse_urlish_host(url);
    if !host.is_empty() || url.contains("://") {
        return normalize_hostname(&host);
    }
    normalize_hostname(&parse_urlish_host(&format!("//{url}")))
}

/// Lightweight equivalent of Python `urlparse(s).hostname or urlparse(s).netloc`.
fn parse_urlish_host(url: &str) -> String {
    let cleaned: String = url
        .trim_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c'])
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let rest = if let Some((_, after)) = cleaned.split_once("://") {
        after
    } else if let Some(after) = cleaned.strip_prefix("//") {
        after
    } else {
        return String::new();
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    if netloc.is_empty() {
        return String::new();
    }
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    if let Some(after_open) = hostinfo.strip_prefix('[') {
        if let Some((ipv6, _)) = after_open.split_once(']') {
            return ipv6.to_owned();
        }
        return netloc.to_owned();
    }
    hostinfo.split(':').next().unwrap_or("").to_owned()
}

/// `*.example.com` rules glob-match (case-insensitive `fnmatch`); bare hosts match
/// exactly or as a parent domain. Mirrors `tools/website_policy.py::_match_host_against_rule`.
pub fn match_host_against_rule(host: &str, pattern: &str) -> bool {
    if host.is_empty() || pattern.is_empty() {
        return false;
    }
    if pattern.starts_with("*.") {
        return glob_match(pattern, host);
    }
    host == pattern
        || (host.len() > pattern.len()
            && host.ends_with(pattern)
            && host.as_bytes()[host.len() - pattern.len() - 1] == b'.')
}

/// Standard POSIX `fnmatch`-style wildcard matcher (`*`, `?`, `[...]`).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);
    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(raw)
}

fn read_blocklist_file_rules(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let text = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    text.lines().filter_map(normalize_rule).collect()
}

impl WebsitePolicy {
    /// Load `security.website_blocklist` from parsed YAML config.
    /// Fails open (returns disabled default) on malformed field types, matching upstream runtime.
    pub fn from_config(config: &serde_yaml::Value, home: Option<&Path>) -> Self {
        let Some(security) = config.get("security") else {
            return Self::default();
        };
        if security.is_null() {
            return Self::default();
        }
        let Some(security_map) = security.as_mapping() else {
            return Self::default();
        };
        let Some(blocklist) =
            security_map.get(serde_yaml::Value::String("website_blocklist".into()))
        else {
            return Self::default();
        };
        if blocklist.is_null() {
            return Self::default();
        }
        let Some(bl_map) = blocklist.as_mapping() else {
            return Self::default();
        };

        // `_DEFAULT_WEBSITE_BLOCKLIST` has `enabled: False`, so absent `enabled` -> false.
        let enabled = match bl_map.get(serde_yaml::Value::String("enabled".into())) {
            None => false,
            Some(serde_yaml::Value::Bool(b)) => *b,
            Some(_) => return Self::default(),
        };

        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut rules: Vec<WebsiteRule> = Vec::new();
        let mut push_rule = |pattern: String, source: String| {
            if seen.insert((pattern.clone(), source.clone())) {
                rules.push(WebsiteRule { pattern, source });
            }
        };

        match bl_map.get(serde_yaml::Value::String("domains".into())) {
            None | Some(serde_yaml::Value::Null) => {}
            Some(serde_yaml::Value::Sequence(items)) => {
                for item in items {
                    if let Some(s) = item.as_str() {
                        if let Some(norm) = normalize_rule(s) {
                            push_rule(norm, "config".into());
                        }
                    }
                }
            }
            Some(_) => return Self::default(),
        }

        match bl_map.get(serde_yaml::Value::String("shared_files".into())) {
            None | Some(serde_yaml::Value::Null) => {}
            Some(serde_yaml::Value::Sequence(items)) => {
                for item in items {
                    let Some(raw_path) = item.as_str().map(str::trim).filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let expanded = expand_tilde(raw_path);
                    let resolved = if expanded.is_absolute() {
                        expanded
                    } else if let Some(home_dir) = home {
                        let joined = home_dir.join(&expanded);
                        joined.canonicalize().unwrap_or(joined)
                    } else {
                        expanded.canonicalize().unwrap_or(expanded)
                    };
                    let source = resolved.display().to_string();
                    for norm in read_blocklist_file_rules(&resolved) {
                        push_rule(norm, source.clone());
                    }
                }
            }
            Some(_) => return Self::default(),
        }

        Self { enabled, rules }
    }

    /// Return `Some(WebsiteBlock)` if `url` is blocked by the policy, else `None`.
    pub fn check_access(&self, url: &str) -> Option<WebsiteBlock> {
        if !self.enabled {
            return None;
        }
        let host = extract_host_from_urlish(url);
        if host.is_empty() {
            return None;
        }
        for rule in &self.rules {
            if match_host_against_rule(&host, &rule.pattern) {
                return Some(WebsiteBlock {
                    url: url.to_owned(),
                    host: host.clone(),
                    rule: rule.pattern.clone(),
                    source: rule.source.clone(),
                    message: format!(
                        "Blocked by website policy: '{host}' matched rule '{}' from {}",
                        rule.pattern, rule.source
                    ),
                });
            }
        }
        None
    }
}
