//! Change-detecting cron monitors (`monitor_script` / `monitor_url`).
//!
//! Mirrors `cron/monitor.py` of the Python reference:
//! - hashes exact UTF-8 output bytes with SHA-256;
//! - skips the agent silently (`changed = false`) when the hash matches `job["monitor_state"]`;
//! - builds a unified diff (`--- previous` / `+++ current`, capped at 4000 chars) when changed,
//!   persists `monitor_last_output.txt` and `job["monitor_state"]`, and returns the prompt context
//!   block;
//! - enforces SSRF IP blocking and `website_policy` blocklist checks on `monitor_url` fetches and
//!   every redirect hop (up to 5 hops, 256 KiB cap, 30s timeout).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use ring::digest;
use serde_json::{json, Value};

use super::schedule::iso_instant;
use super::script::{run_job_script, script_timeout_secs};
use super::store::{cron_dir, with_jobs};
use crate::website_policy::WebsitePolicy;

pub const MAX_DIFF_CHARS: usize = 4000;
pub const MAX_OUTPUT_CHARS: usize = 8000;
pub const MAX_URL_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 5;
const URL_TIMEOUT_SECS: u64 = 30;
const LAST_OUTPUT_FILENAME: &str = "monitor_last_output.txt";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorResult {
    pub ok: bool,
    pub changed: bool,
    pub first_run: bool,
    pub current_hash: String,
    pub context_block: String,
    pub error: Option<String>,
}

/// Whether `job` has a monitor source configured.
pub fn job_has_monitor(job: &Value) -> bool {
    let has = |key: &str| {
        job[key]
            .as_str()
            .map(str::trim)
            .is_some_and(|text| !text.is_empty())
    };
    has("monitor_script") || has("monitor_url")
}

/// Split the model-facing `monitor` argument into `(monitor_script, monitor_url)`.
/// Matches `cron/job_args.py::_split_monitor_arg`.
pub fn split_monitor_arg(
    monitor: Option<&str>,
    monitor_script: Option<&str>,
    monitor_url: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(raw) = monitor else {
        return (
            monitor_script.map(str::to_owned),
            monitor_url.map(str::to_owned),
        );
    };
    let value = raw.trim();
    if value.is_empty() {
        return (Some(String::new()), Some(String::new()));
    }
    let lower = value.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return (Some(String::new()), Some(value.to_owned()));
    }
    (Some(value.to_owned()), Some(String::new()))
}

/// SHA-256 hex digest of the exact UTF-8 monitor output.
pub fn hash_monitor_output(output: &str) -> String {
    let sum = digest::digest(&digest::SHA256, output.as_bytes());
    sum.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Build a unified diff between `previous` and `current`, matching `difflib.unified_diff`
/// (`fromfile="previous"`, `tofile="current"`, `n=3`, `lineterm=""`) capped at `max_chars`.
pub fn build_monitor_diff(previous: &str, current: &str, max_chars: usize) -> String {
    let old_lines: Vec<&str> = split_lines_python(previous);
    let new_lines: Vec<&str> = split_lines_python(current);
    let ops = diff_opcodes(&old_lines, &new_lines);
    let groups = group_opcodes(&ops, 3);
    if groups.is_empty() {
        return String::new();
    }
    let mut out = vec!["--- previous".to_owned(), "+++ current".to_owned()];
    for group in groups {
        let first = group[0];
        let last = group[group.len() - 1];
        let file1_range = format_range_unified(first.i1, last.i2);
        let file2_range = format_range_unified(first.j1, last.j2);
        out.push(format!("@@ -{file1_range} +{file2_range} @@"));
        for op in group {
            match op.tag {
                DiffTag::Equal => {
                    for line in &old_lines[op.i1..op.i2] {
                        out.push(format!(" {line}"));
                    }
                }
                DiffTag::Replace | DiffTag::Delete => {
                    for line in &old_lines[op.i1..op.i2] {
                        out.push(format!("-{line}"));
                    }
                    if op.tag == DiffTag::Replace {
                        for line in &new_lines[op.j1..op.j2] {
                            out.push(format!("+{line}"));
                        }
                    }
                }
                DiffTag::Insert => {
                    for line in &new_lines[op.j1..op.j2] {
                        out.push(format!("+{line}"));
                    }
                }
            }
        }
    }
    let joined = out.join("\n");
    if joined.chars().count() > max_chars {
        let prefix: String = joined.chars().take(max_chars).collect();
        format!("{prefix}\n... [diff truncated]")
    } else {
        joined
    }
}

fn split_lines_python(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().collect()
}

fn format_range_unified(start: usize, stop: usize) -> String {
    let mut beginning = start + 1;
    let length = stop - start;
    if length == 1 {
        return beginning.to_string();
    }
    if length == 0 {
        beginning -= 1;
    }
    format!("{beginning},{length}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTag {
    Equal,
    Replace,
    Delete,
    Insert,
}

#[derive(Clone, Copy, Debug)]
struct Opcode {
    tag: DiffTag,
    i1: usize,
    i2: usize,
    j1: usize,
    j2: usize,
}

/// Compute LCS-based opcodes between `a` and `b` (falls back to single Replace when huge to stay
/// O(1) memory/time for very large monitor payloads).
fn diff_opcodes(a: &[&str], b: &[&str]) -> Vec<Opcode> {
    let m = a.len();
    let n = b.len();
    if a == b {
        return if m == 0 {
            Vec::new()
        } else {
            vec![Opcode {
                tag: DiffTag::Equal,
                i1: 0,
                i2: m,
                j1: 0,
                j2: n,
            }]
        };
    }
    // Trim common prefix and suffix first.
    let mut prefix = 0;
    while prefix < m && prefix < n && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (m - prefix) && suffix < (n - prefix) && a[m - 1 - suffix] == b[n - 1 - suffix] {
        suffix += 1;
    }
    let a_mid = &a[prefix..m - suffix];
    let b_mid = &b[prefix..n - suffix];
    let mut ops = Vec::new();
    if prefix > 0 {
        ops.push(Opcode {
            tag: DiffTag::Equal,
            i1: 0,
            i2: prefix,
            j1: 0,
            j2: prefix,
        });
    }
    if a_mid.len() * b_mid.len() > 250_000 {
        if !a_mid.is_empty() && !b_mid.is_empty() {
            ops.push(Opcode {
                tag: DiffTag::Replace,
                i1: prefix,
                i2: m - suffix,
                j1: prefix,
                j2: n - suffix,
            });
        } else if !a_mid.is_empty() {
            ops.push(Opcode {
                tag: DiffTag::Delete,
                i1: prefix,
                i2: m - suffix,
                j1: prefix,
                j2: prefix,
            });
        } else if !b_mid.is_empty() {
            ops.push(Opcode {
                tag: DiffTag::Insert,
                i1: prefix,
                i2: prefix,
                j1: prefix,
                j2: n - suffix,
            });
        }
    } else {
        // Standard DP LCS table on middle slice.
        let am = a_mid.len();
        let bn = b_mid.len();
        let mut dp = vec![vec![0u32; bn + 1]; am + 1];
        for i in (0..am).rev() {
            for j in (0..bn).rev() {
                dp[i][j] = if a_mid[i] == b_mid[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        let mut matches = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < am && j < bn {
            if a_mid[i] == b_mid[j] {
                let start_i = i;
                let start_j = j;
                while i < am && j < bn && a_mid[i] == b_mid[j] {
                    i += 1;
                    j += 1;
                }
                matches.push((prefix + start_i, prefix + start_j, i - start_i));
            } else if dp[i + 1][j] >= dp[i][j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        matches.push((m - suffix, n - suffix, 0));
        let (mut cur_i, mut cur_j) = (prefix, prefix);
        for (mi, mj, size) in matches {
            if cur_i < mi && cur_j < mj {
                ops.push(Opcode {
                    tag: DiffTag::Replace,
                    i1: cur_i,
                    i2: mi,
                    j1: cur_j,
                    j2: mj,
                });
            } else if cur_i < mi {
                ops.push(Opcode {
                    tag: DiffTag::Delete,
                    i1: cur_i,
                    i2: mi,
                    j1: cur_j,
                    j2: mj,
                });
            } else if cur_j < mj {
                ops.push(Opcode {
                    tag: DiffTag::Insert,
                    i1: cur_i,
                    i2: mi,
                    j1: cur_j,
                    j2: mj,
                });
            }
            if size > 0 {
                ops.push(Opcode {
                    tag: DiffTag::Equal,
                    i1: mi,
                    i2: mi + size,
                    j1: mj,
                    j2: mj + size,
                });
            }
            cur_i = mi + size;
            cur_j = mj + size;
        }
    }
    if suffix > 0 {
        ops.push(Opcode {
            tag: DiffTag::Equal,
            i1: m - suffix,
            i2: m,
            j1: n - suffix,
            j2: n,
        });
    }
    ops
}

fn group_opcodes(ops: &[Opcode], n: usize) -> Vec<Vec<Opcode>> {
    if ops.is_empty() || (ops.len() == 1 && ops[0].tag == DiffTag::Equal) {
        return Vec::new();
    }
    let mut codes = ops.to_vec();
    if codes[0].tag == DiffTag::Equal {
        let first = &mut codes[0];
        first.i1 = first.i1.max(first.i2.saturating_sub(n));
        first.j1 = first.j1.max(first.j2.saturating_sub(n));
    }
    if let Some(last) = codes.last_mut() {
        if last.tag == DiffTag::Equal {
            last.i2 = last.i2.min(last.i1 + n);
            last.j2 = last.j2.min(last.j1 + n);
        }
    }
    let nn = n + n;
    let mut groups = Vec::new();
    let mut group = Vec::new();
    for op in codes {
        if op.tag == DiffTag::Equal && op.i2 - op.i1 > nn {
            group.push(Opcode {
                tag: DiffTag::Equal,
                i1: op.i1,
                i2: op.i2.min(op.i1 + n),
                j1: op.j1,
                j2: op.j2.min(op.j1 + n),
            });
            groups.push(group);
            group = Vec::new();
            let i1 = op.i1.max(op.i2.saturating_sub(n));
            let j1 = op.j1.max(op.j2.saturating_sub(n));
            group.push(Opcode {
                tag: DiffTag::Equal,
                i1,
                i2: op.i2,
                j1,
                j2: op.j2,
            });
        } else {
            group.push(op);
        }
    }
    if !(group.is_empty() || (group.len() == 1 && group[0].tag == DiffTag::Equal)) {
        groups.push(group);
    }
    groups
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

async fn validate_monitor_url(home: &Path, url_str: &str) -> Result<reqwest::Url, String> {
    let url =
        reqwest::Url::parse(url_str).map_err(|error| format!("Invalid monitor URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "Monitor URL must use http:// or https://, got scheme '{}'",
            url.scheme()
        ));
    }
    let config =
        crate::config::read_config(&home.join("config.yaml")).unwrap_or(serde_yaml::Value::Null);
    let policy = WebsitePolicy::from_config(&config, Some(home));
    if let Some(block) = policy.check_access(url_str) {
        return Err(block.message);
    }
    let host = url
        .host_str()
        .ok_or_else(|| "Monitor URL has no host".to_owned())?;
    let allow_private = std::env::var("HERMES_ALLOW_PRIVATE_MONITOR_URLS")
        .ok()
        .is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"));
    if !allow_private {
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(format!(
                    "Blocked monitor URL host (private/loopback): {host}"
                ));
            }
        } else {
            let port = url.port_or_known_default().unwrap_or(80);
            let addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|error| format!("Monitor URL DNS lookup failed for {host}: {error}"))?;
            for addr in addrs {
                if is_blocked_ip(addr.ip()) {
                    return Err(format!(
                        "Blocked monitor URL host {host} resolving to private IP {}",
                        addr.ip()
                    ));
                }
            }
        }
    }
    Ok(url)
}

async fn fetch_monitor_url(home: &Path, initial_url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(URL_TIMEOUT_SECS))
        .user_agent("HermesMonitor/1.0")
        .build()
        .map_err(|error| format!("Failed to build HTTP client: {error}"))?;

    let mut current = validate_monitor_url(home, initial_url).await?;
    for _ in 0..=MAX_REDIRECTS {
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|error| format!("Monitor URL fetch failed: {error}"))?;
        let status = response.status();
        if status.is_redirection() {
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(format!(
                    "Monitor URL returned redirect {status} without Location"
                ));
            };
            let next = current
                .join(location)
                .map_err(|error| format!("Invalid redirect URL: {error}"))?;
            current = validate_monitor_url(home, next.as_str()).await?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("Monitor URL returned HTTP {}", status.as_u16()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("Failed to read monitor URL body: {error}"))?;
        let slice = if bytes.len() > MAX_URL_BYTES {
            &bytes[..MAX_URL_BYTES]
        } else {
            &bytes
        };
        let text = String::from_utf8_lossy(slice);
        return Ok(crate::redact::redact_with(
            text.trim(),
            crate::redact::Mode::default(),
        ));
    }
    Err(format!("Monitor URL exceeded {MAX_REDIRECTS} redirects"))
}

fn last_output_path(home: &Path, job_id: &str) -> Result<PathBuf, String> {
    if job_id.is_empty() || job_id == "." || job_id == ".." || job_id.contains(['/', '\\']) {
        return Err(format!(
            "Invalid cron job id for monitor output: {job_id:?}"
        ));
    }
    Ok(cron_dir(home)
        .join("output")
        .join(job_id)
        .join(LAST_OUTPUT_FILENAME))
}

fn clip_output(text: &str) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        text.to_owned()
    } else {
        let prefix: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        format!("{prefix}\n... [output truncated]")
    }
}

/// Evaluate a job's monitor source, compare its hash to `job["monitor_state"]`, update state on
/// change, and return a `MonitorResult`.
pub async fn check_monitor<F>(
    home: &Path,
    job: &Value,
    now: DateTime<Tz>,
    cancel_reason: F,
) -> MonitorResult
where
    F: FnMut() -> Option<String>,
{
    let job_id = job["id"].as_str().unwrap_or_default();
    let monitor_script = job["monitor_script"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let monitor_url = job["monitor_url"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let (source_label, output) = if let Some(script) = monitor_script {
        let workdir = job["workdir"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Path::new);
        let interp = job["interpreter"].as_str();
        let (ok, out) = run_job_script(
            home,
            script,
            workdir,
            interp,
            script_timeout_secs(),
            cancel_reason,
        )
        .await;
        if !ok {
            return MonitorResult {
                ok: false,
                changed: false,
                first_run: false,
                current_hash: String::new(),
                context_block: String::new(),
                error: Some(out),
            };
        }
        (format!("script `{script}`"), out)
    } else if let Some(url) = monitor_url {
        match fetch_monitor_url(home, url).await {
            Ok(out) => (format!("URL `{url}`"), out),
            Err(error) => {
                return MonitorResult {
                    ok: false,
                    changed: false,
                    first_run: false,
                    current_hash: String::new(),
                    context_block: String::new(),
                    error: Some(error),
                };
            }
        }
    } else {
        return MonitorResult {
            ok: false,
            changed: false,
            first_run: false,
            current_hash: String::new(),
            context_block: String::new(),
            error: Some("No monitor source configured".into()),
        };
    };

    let current_hash = hash_monitor_output(&output);
    let previous_hash = job["monitor_state"]["last_output_hash"]
        .as_str()
        .filter(|h| !h.is_empty());

    if previous_hash == Some(current_hash.as_str()) {
        return MonitorResult {
            ok: true,
            changed: false,
            first_run: false,
            current_hash,
            context_block: String::new(),
            error: None,
        };
    }

    let first_run = previous_hash.is_none();
    let previous_output = last_output_path(home, job_id)
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();

    // Persist the new snapshot and monitor_state before waking the agent.
    if let Ok(path) = last_output_path(home, job_id) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, &output);
    }
    let changed_at = iso_instant(now);
    let _ = with_jobs(home, now, |jobs| {
        if let Some(stored) = jobs.iter_mut().find(|j| j["id"].as_str() == Some(job_id)) {
            stored["monitor_state"] = json!({
                "last_output_hash": current_hash,
                "last_changed_at": changed_at,
            });
            return Ok(((), true));
        }
        Ok(((), false))
    });

    let clipped = clip_output(&output);
    let context_block = if first_run {
        format!(
            "## Monitor Baseline (first run)\n\
             Monitored {source_label}. This is the initial baseline snapshot:\n\n\
             ```\n{clipped}\n```"
        )
    } else {
        let diff = build_monitor_diff(&previous_output, &output, MAX_DIFF_CHARS);
        format!(
            "## MONITOR CHANGE DETECTED\n\
             Monitored {source_label} changed since the previous tick.\n\n\
             ### Diff\n```diff\n{diff}\n```\n\n\
             ### Current Output\n```\n{clipped}\n```"
        )
    };

    MonitorResult {
        ok: true,
        changed: true,
        first_run,
        current_hash,
        context_block,
        error: None,
    }
}
