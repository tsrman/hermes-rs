//! The `session_search` tool: recall over the profile's session store, with no model calls.
//!
//! Follows `tools/session_search_tool.py`. One call has one of four shapes, picked by its
//! arguments: discovery (`query`: ranked sessions, the top one fully hydrated), scroll
//! (`session_id` with `around_message_id`), read (`session_id` alone) and browse (no
//! arguments). Rows that surface from a query, and every title and preview, pass through
//! `redact_derived_text`; scroll and read return the stored text of a session the caller
//! named, as the reference does.
//!
//! Not ported: the confinement of cross-profile reads for multiplexed hosts (the CLI serves
//! one profile and its operator), and the cause-specific text of an unavailable store.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::redact::redact_derived_text;
use crate::storage::recall::{
    Hit, RecallStore, SearchRequest, SessionMeta, Stamp, StoredMessage, Visibility,
    BOUNDARY_END_REASONS,
};

pub(crate) const SESSION_SEARCH: &str = "session_search";

/// Hidden from browsing and searching: integrations, delegate subagents and kanban workers.
const HIDDEN_SOURCES: [&str; 3] = ["kanban", "subagent", "tool"];
/// Searchable, but ranked below interactive sessions.
const DEMOTED_SOURCES: [&str; 1] = ["cron"];
const READ_MAX_CONTENT: usize = 2000;
const DISCOVER_SCAN_LIMIT: usize = 300;
const EXCLUDE_SESSION_IDS_CAP: usize = 20;
const COMPACTION_PREFIXES: [&str; 2] = ["[CONTEXT COMPACTION", "[CONTEXT SUMMARY]:"];
const DATA_URL_PLACEHOLDER: &str = "[inline data omitted]";
const SNIPPET_BEFORE: usize = 100;
const SNIPPET_AFTER: usize = 200;
const PREVIEW_MAX_CHARS: usize = 60;
const PROFILE_RULE: &str =
    "Use lowercase letters, numbers, '-' or '_', starting with a letter or number, up to 64 characters";
const RESERVED_PROFILE_NAMES: [&str; 6] = ["hermes", "default", "test", "tmp", "root", "sudo"];
const PROFILE_IDENTITY_MARKERS: [&str; 6] = [
    "config.yaml",
    ".env",
    "SOUL.md",
    "profile.yaml",
    "auth.json",
    "state.db",
];
const DELETED_PROFILES_DIR: &str = ".deleted";

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/session_search.json")).unwrap_or_default()
    })
}

fn error(message: impl AsRef<str>) -> Value {
    let message = message.as_ref();
    let bounded = if message.chars().count() > 2048 {
        format!(
            "{}… [truncated]",
            message.chars().take(2048).collect::<String>()
        )
    } else {
        message.to_owned()
    };
    json!({"error": bounded, "success": false})
}

fn ok(payload: Vec<(&str, Value)>) -> Value {
    let mut out = Map::new();
    out.insert("success".into(), Value::Bool(true));
    for (key, value) in payload {
        out.insert(key.into(), value);
    }
    Value::Object(out)
}

// ---- argument coercion, as Python's `int()` and `str()` read loosely typed input ----

fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.trunc() as i64)
        }),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn clamp_int(value: Option<&Value>, default: i64, low: i64, high: i64) -> i64 {
    value.and_then(py_int).unwrap_or(default).clamp(low, high)
}

/// Python's `repr()` of a string.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

// ---- time ----

fn host_zone() -> Tz {
    std::env::var("TZ")
        .ok()
        .and_then(|name| name.trim_start_matches(':').parse().ok())
        .or_else(|| iana_time_zone::get_timezone().ok()?.parse().ok())
        .unwrap_or(Tz::UTC)
}

fn format_number_stamp(seconds: f64) -> Option<String> {
    if !seconds.is_finite() {
        return None;
    }
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9) as u32;
    let instant = DateTime::<Utc>::from_timestamp(whole as i64, nanos)?;
    Some(
        instant
            .with_timezone(&host_zone())
            .format("%B %d, %Y at %I:%M %p")
            .to_string(),
    )
}

/// A stored timestamp as readable text; text that is not a number passes through.
fn format_timestamp(stamp: Option<&Stamp>) -> String {
    match stamp {
        None => "unknown".into(),
        Some(Stamp::Number(seconds)) => {
            format_number_stamp(*seconds).unwrap_or_else(|| py_float_text(*seconds))
        }
        Some(Stamp::Text(text)) => {
            let digits = text.replace(['.', '-'], "");
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                text.parse::<f64>()
                    .ok()
                    .and_then(format_number_stamp)
                    .unwrap_or_else(|| text.clone())
            } else {
                text.clone()
            }
        }
    }
}

fn py_float_text(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

static RELATIVE_BOUND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\d+)\s*(h|d|w)$").expect("valid pattern"));

/// An ISO date or datetime, or a relative duration (`7d`, `24h`, `2w` = now minus that), as a
/// UTC unix timestamp. A date-only value is midnight UTC.
fn parse_bound(value: Option<&Value>, now: i64) -> Result<Option<i64>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = match value {
        Value::String(text) => text.trim().to_owned(),
        other => other.to_string(),
    };
    if text.is_empty() {
        return Ok(None);
    }
    if let Some(rel) = RELATIVE_BOUND.captures(&text) {
        let count: i64 = rel[1].parse().unwrap_or(0);
        let unit = match rel[2].to_lowercase().as_str() {
            "h" => 3600,
            "d" => 86400,
            _ => 604_800,
        };
        return Ok(Some(now - count * unit));
    }
    parse_iso(&text).map(Some).ok_or_else(|| {
        format!(
            "invalid time bound: {} (expected ISO date/datetime like 2026-07-01, or a relative duration like 7d, 24h, 2w)",
            py_repr(&value.as_str().map_or_else(|| value.to_string(), str::to_owned))
        )
    })
}

/// `datetime.fromisoformat`, with a naive value read as UTC.
fn parse_iso(text: &str) -> Option<i64> {
    use chrono::{NaiveDate, NaiveDateTime};
    let normalized = text.replace('Z', "+00:00");
    if let Ok(parsed) = DateTime::parse_from_rfc3339(&normalized.replacen(' ', "T", 1)) {
        return Some(parsed.timestamp());
    }
    for layout in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%dT%H%:z",
    ] {
        if let Ok(parsed) = DateTime::parse_from_str(&normalized.replacen(' ', "T", 1), layout) {
            return Some(parsed.timestamp());
        }
    }
    let naive = normalized.replacen(' ', "T", 1);
    for layout in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(&naive, layout) {
            return Some(parsed.and_utc().timestamp());
        }
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp())
}

fn started_ts(stamp: Option<&Stamp>, now: i64) -> Option<i64> {
    match stamp? {
        Stamp::Number(seconds) => Some(*seconds as i64),
        Stamp::Text(text) => {
            let text = text.trim();
            text.parse::<f64>().ok().map(|f| f as i64).or_else(|| {
                parse_bound(Some(&Value::String(text.to_owned())), now)
                    .ok()
                    .flatten()
            })
        }
    }
}

fn in_time_window(started: Option<i64>, after: Option<i64>, before: Option<i64>) -> bool {
    if after.is_none() && before.is_none() {
        return true;
    }
    let Some(started) = started else { return false };
    after.is_none_or(|after| started >= after) && before.is_none_or(|before| started < before)
}

// ---- text handling ----

static ANSI_ESCAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)\x1b(?:\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\][\s\S]*?(?:\x07|\x1b\\)|[PX^_][\s\S]*?(?:\x1b\\)|[\x20-\x2f]+[\x30-\x7e]|[\x30-\x7e])|\x{9b}[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x{9d}[\s\S]*?(?:\x07|\x{9c})|[\x{80}-\x{9f}]",
    )
    .expect("valid pattern")
});

fn strip_ansi(text: &str) -> String {
    ANSI_ESCAPE.replace_all(text, "").into_owned()
}

static DATA_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^data:[^\s,]*;base64,").expect("valid pattern"));

/// Redact every string leaf of stored message structure, keeping its shape. Inline `data:`
/// URLs are replaced outright: they are opaque to the redactor and never useful in recall.
fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => {
            if DATA_URL.is_match(text) {
                Value::String(DATA_URL_PLACEHOLDER.into())
            } else {
                Value::String(redact_derived_text(text))
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Each call's `function.arguments`, redacted field-wise so the JSON shape survives.
fn redact_tool_calls(calls: &Value) -> Value {
    let Value::Array(items) = calls else {
        return redact_value(calls);
    };
    Value::Array(
        items
            .iter()
            .map(|call| {
                let Some(function) = call.get("function").filter(|f| f.is_object()) else {
                    return call.clone();
                };
                let Some(arguments) = function.get("arguments").filter(|a| !a.is_null()) else {
                    return call.clone();
                };
                let mut function = function.clone();
                function["arguments"] = redact_value(arguments);
                let mut call = call.clone();
                call["function"] = function;
                call
            })
            .collect(),
    )
}

fn content_text(content: &Value) -> String {
    match content {
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn is_compaction_summary(content: &Value) -> bool {
    let Value::String(text) = content else {
        return false;
    };
    let trimmed = text.trim_start();
    COMPACTION_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// A stored row slimmed for output. `derived` rows resurface across sessions, so they get the
/// strict redaction.
fn shape_message(
    message: &StoredMessage,
    anchor: Option<i64>,
    max_content: Option<usize>,
    derived: bool,
) -> Value {
    let mut content = message.content.clone();
    if let Value::String(text) = &content {
        if text.contains('\x1b') {
            content = Value::String(strip_ansi(text));
        }
    }
    if derived {
        content = redact_value(&content);
    }
    let mut entry = Map::new();
    entry.insert("id".into(), json!(message.id));
    entry.insert("role".into(), json!(message.role));
    entry.insert("content".into(), content.clone());
    entry.insert("timestamp".into(), json!(message.timestamp));
    if let Some(name) = message.tool_name.as_deref().filter(|n| !n.is_empty()) {
        entry.insert("tool_name".into(), json!(name));
    }
    if !matches!(&message.tool_calls, Value::Null) && !is_falsy(&message.tool_calls) {
        let calls = if derived {
            redact_tool_calls(&message.tool_calls)
        } else {
            message.tool_calls.clone()
        };
        entry.insert("tool_calls".into(), calls);
    }
    if let Some(id) = message.tool_call_id.as_deref().filter(|n| !n.is_empty()) {
        entry.insert("tool_call_id".into(), json!(id));
    }
    if anchor == Some(message.id) {
        entry.insert("anchor".into(), Value::Bool(true));
    }
    if let (Some(limit), Value::String(text)) = (max_content, &content) {
        let length = text.chars().count();
        if length > limit {
            let cut: String = text.chars().take(limit).collect();
            entry.insert("content".into(), json!(format!("{cut}…")));
            entry.insert("content_truncated".into(), Value::Bool(true));
            entry.insert("original_content_chars".into(), json!(length));
        }
    }
    Value::Object(entry)
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

// ---- the call ----

pub(crate) struct Recall<'a> {
    /// The profile directory the call runs in.
    pub home: &'a Path,
    /// The session this turn belongs to, when it is saved.
    pub session_id: Option<&'a str>,
}

struct Scope<'a> {
    store: &'a RecallStore,
    current_session: Option<&'a str>,
    link_profile: Option<String>,
    link_name: String,
}

pub(crate) fn call(recall: &Recall<'_>, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let get = |key: &str| args.get(key).filter(|v| !v.is_null());
    let mut session_id = get("session_id").and_then(Value::as_str).map(str::to_owned);
    let mut profile = get("profile").and_then(Value::as_str).map(str::to_owned);
    // A raw `@session:<profile>/<id>` link: ids never contain "/", so split on it.
    if let Some(raw) = session_id.clone().filter(|id| id.contains('/')) {
        let (embedded_profile, embedded_id) = raw.split_once('/').unwrap_or((&raw, ""));
        if !embedded_id.is_empty() {
            session_id = Some(embedded_id.to_owned());
            if !embedded_profile.is_empty()
                && profile.as_deref().is_none_or(|p| p.trim().is_empty())
            {
                profile = Some(embedded_profile.to_owned());
            }
        }
    }
    let other_store;
    let mut current_session = recall.session_id;
    let named = profile.as_deref().filter(|p| !p.trim().is_empty());
    let store_ref;
    let store = if let Some(name) = named {
        match open_profile(recall.home, name) {
            Ok(store) => {
                other_store = store;
                current_session = None;
                &other_store
            }
            Err(message) => return error(format!("profile '{name}': {message}")),
        }
    } else {
        match RecallStore::open(recall.home) {
            Ok(store) => {
                store_ref = store;
                &store_ref
            }
            Err(_) => return error("Hermes can't open its session history right now."),
        }
    };
    let scope = Scope {
        store,
        current_session,
        link_profile: named.map(str::to_owned),
        link_name: active_profile_name(recall.home),
    };
    dispatch(&scope, &args, session_id.as_deref())
}

fn dispatch(scope: &Scope<'_>, args: &Value, session_id: Option<&str>) -> Value {
    let get = |key: &str| args.get(key).filter(|v| !v.is_null());
    if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
        if let Some(anchor) = get("around_message_id") {
            return scroll(scope, id, anchor, args.get("window"));
        }
        return read_scoped(scope, id);
    }
    let query = get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty());
    let browse = query.is_none();
    let limit = clamp_int(args.get("limit"), if browse { 10 } else { 3 }, 1, 10) as usize;
    let Some(query) = query else {
        return list_recent(scope, limit);
    };
    let sort = get("sort")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase());
    let sort = sort.filter(|s| s == "newest" || s == "oldest");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (after, before) = match (
        parse_bound(get("after"), now),
        parse_bound(get("before"), now),
    ) {
        (Ok(after), Ok(before)) => (after, before),
        (Err(message), _) | (_, Err(message)) => return error(message),
    };
    let roles: Vec<String> = get("role_filter")
        .and_then(Value::as_str)
        .map(|text| {
            text.split(',')
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let detail_full = get("detail")
        .or(None)
        .and_then(Value::as_str)
        .is_some_and(|d| d.trim().eq_ignore_ascii_case("full"));
    discover(
        scope,
        &Discover {
            query,
            roles: if roles.is_empty() {
                vec!["user".into(), "assistant".into()]
            } else {
                roles
            },
            limit,
            sort: sort.as_deref(),
            full: detail_full,
            after,
            before,
            exclude: normalize_exclude(args.get("exclude_session_ids")),
        },
        now,
    )
}

fn normalize_exclude(raw: Option<&Value>) -> Vec<String> {
    let items: Vec<&Value> = match raw {
        Some(Value::String(_)) => vec![raw.expect("some")],
        Some(Value::Array(items)) => items.iter().collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let id = item.as_str().map_or("", str::trim);
        if !id.is_empty() && !out.iter().any(|seen| seen == id) {
            out.push(id.to_owned());
        }
        if out.len() >= EXCLUDE_SESSION_IDS_CAP {
            break;
        }
    }
    out
}

// ---- profiles ----

fn profile_root(home: &Path) -> PathBuf {
    match home.parent() {
        Some(parent) if parent.file_name().is_some_and(|name| name == "profiles") => {
            parent.parent().unwrap_or(home).to_path_buf()
        }
        _ => home.to_path_buf(),
    }
}

/// `default` for the root profile, the name under `profiles/`, `custom` for anything else.
fn active_profile_name(home: &Path) -> String {
    if let Some(name) = home
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|n| n == "profiles"))
        .and_then(|_| home.file_name()?.to_str())
    {
        return name.to_owned();
    }
    let default_home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hermes"));
    let resolved = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if default_home.is_some_and(|d| d.canonicalize().unwrap_or(d) == resolved) {
        "default".into()
    } else {
        "custom".into()
    }
}

fn valid_profile_id(name: &str) -> bool {
    name.len() <= 64
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'_' | b'-'))
        })
        && !name.is_empty()
}

fn invalid_profile_message(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let mut candidate = String::new();
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            if pending_dash {
                candidate.push('-');
                pending_dash = false;
            }
            candidate.push(c);
        } else {
            pending_dash = true;
        }
    }
    if pending_dash {
        candidate.push('-');
    }
    let candidate: String = candidate
        .trim_matches(['-', '_'])
        .chars()
        .take(64)
        .collect();
    let suggestion = if valid_profile_id(&candidate) {
        candidate
    } else {
        "my-work".into()
    };
    format!(
        "{} is not a valid profile name. {PROFILE_RULE} (for example: {suggestion}). Then run `hermes profile create {suggestion}`.",
        py_repr(name)
    )
}

/// Another profile's store, opened read-only.
fn open_profile(home: &Path, name: &str) -> Result<RecallStore, String> {
    let stripped = name.trim();
    let canon = if stripped.to_lowercase() == "default" {
        "default".to_owned()
    } else {
        stripped.to_lowercase()
    };
    if canon != "default" {
        if !valid_profile_id(&canon) {
            return Err(invalid_profile_message(&canon));
        }
        if RESERVED_PROFILE_NAMES.contains(&canon.as_str()) {
            return Err(format!(
                "Profile name {} is reserved — it collides with either the Hermes installation itself or a common system binary.  Pick a different name.",
                py_repr(&canon)
            ));
        }
    }
    let root = profile_root(home);
    let target = if canon == "default" {
        root.clone()
    } else {
        root.join("profiles").join(&canon)
    };
    let exists = canon == "default" || {
        let tombstone = root
            .join("profiles")
            .join(DELETED_PROFILES_DIR)
            .join(&canon);
        target.is_dir()
            && PROFILE_IDENTITY_MARKERS
                .iter()
                .any(|marker| target.join(marker).is_file() || target.join(marker).is_symlink())
            && !tombstone.exists()
    };
    if !exists {
        return Err(format!("profile '{canon}' does not exist"));
    }
    RecallStore::open(&target).map_err(|error| error.to_string())
}

// ---- lineage ----

fn resolve_to_parent(scope: &Scope<'_>, session_id: &str) -> (String, bool) {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut current = session_id.to_owned();
    let mut has_compression = false;
    while !current.is_empty() && !visited.contains(&current) {
        visited.insert(current.clone());
        let meta = scope.store.session(&current);
        has_compression = has_compression
            || meta
                .as_ref()
                .is_some_and(|s| s.end_reason.as_deref() == Some("compression"));
        match meta
            .and_then(|s| s.parent_session_id)
            .filter(|p| !p.is_empty())
        {
            Some(parent) => current = parent,
            None => break,
        }
    }
    (current, has_compression)
}

fn resolve_lineage(scope: &Scope<'_>, session_id: &str) -> String {
    resolve_to_parent(scope, session_id).0
}

fn same_lineage(scope: &Scope<'_>, a: &str, b: &str) -> bool {
    let root = resolve_lineage(scope, a);
    !root.is_empty() && root == resolve_lineage(scope, b)
}

/// The transcript left everyone's live context: compression summarised it into the child, or
/// a fresh reset started the child empty.
fn left_live_context(scope: &Scope<'_>, session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    scope
        .store
        .session(session_id)
        .and_then(|meta| meta.end_reason)
        .is_some_and(|reason| {
            reason == "compression" || BOUNDARY_END_REASONS.contains(&reason.as_str())
        })
}

fn session_link(scope: &Scope<'_>, session_id: &str) -> String {
    let name = scope
        .link_profile
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if scope.link_name == "custom" {
                String::new()
            } else {
                scope.link_name.clone()
            }
        });
    if name.is_empty() {
        format!("@session:{session_id}")
    } else {
        format!("@session:{name}/{session_id}")
    }
}

fn meta_block(meta: &SessionMeta) -> Value {
    json!({
        "when": format_timestamp(meta.started_at.as_ref()),
        "source": meta.source,
        "model": meta.model,
        "title": redact_derived_text(meta.title.as_deref().unwrap_or("")),
    })
}

// ---- read ----

fn read_session(scope: &Scope<'_>, session_id: &str) -> Value {
    let head = 20_usize;
    let tail = 10_usize;
    let Some(meta) = scope.store.session(session_id) else {
        return error(format!("session_id not found: {session_id}"));
    };
    let rows = match scope.store.transcript(session_id) {
        Ok(rows) => rows,
        Err(failure) => return error(format!("failed to load session: {failure}")),
    };
    let shaped: Vec<Value> = rows
        .iter()
        .map(|m| shape_message(m, None, Some(READ_MAX_CONTENT), false))
        .collect();
    let total = shaped.len();
    let truncated = total > head + tail;
    let messages: Vec<Value> = if truncated {
        shaped[..head]
            .iter()
            .chain(shaped[total - tail..].iter())
            .cloned()
            .collect()
    } else {
        shaped
    };
    let mut payload = vec![
        ("mode", json!("read")),
        ("session_id", json!(session_id)),
        ("link", json!(session_link(scope, session_id))),
        ("session_meta", meta_block(&meta)),
        ("message_count", json!(total)),
        ("truncated", json!(truncated)),
        ("messages", Value::Array(messages)),
    ];
    if truncated {
        payload.push((
            "message",
            json!(format!(
                "Session has {total} messages; showing first {head} + last {tail}. Pass around_message_id (any id above) to scroll the middle."
            )),
        ));
    }
    ok(payload)
}

fn read_scoped(scope: &Scope<'_>, session_id: &str) -> Value {
    let result = read_session(scope, session_id);
    if result.get("success") != Some(&Value::Bool(false)) || scope.link_profile.is_some() {
        return result;
    }
    error(format!(
        "session_id not found in this profile: {session_id}. If it belongs to another profile, pass profile=<name> (or the @session:<profile>/<id> link)."
    ))
}

// ---- scroll ----

fn anchor_in_live_context(
    scope: &Scope<'_>,
    state: Option<&(String, Visibility)>,
    anchor_session: &str,
    current: &str,
) -> bool {
    if !same_lineage(scope, anchor_session, current)
        || state.is_some_and(|(_, v)| *v == Visibility::CompactionArchived)
    {
        return false;
    }
    state.is_some_and(|(_, v)| *v != Visibility::Active)
        || !left_live_context(scope, anchor_session)
}

fn scroll(scope: &Scope<'_>, session_id: &str, anchor: &Value, window: Option<&Value>) -> Value {
    let Some(anchor) = py_int(anchor) else {
        return error("scroll requires integer around_message_id");
    };
    let window = clamp_int(window, 5, 1, 20) as usize;
    let state = scope.store.storage_state(anchor);
    let owning = state.as_ref().map(|(session, _)| session.clone());
    if let Some(current) = scope.current_session {
        if anchor_in_live_context(
            scope,
            state.as_ref(),
            owning.as_deref().unwrap_or(session_id),
            current,
        ) {
            return error("scroll rejected: anchor lives in the current session lineage (already in your active context)");
        }
    }
    let Some(mut meta) = scope.store.session(session_id) else {
        return error(format!("session_id not found: {session_id}"));
    };
    let mut session_id = session_id.to_owned();
    let mut view = match scope.store.around(&session_id, anchor, window) {
        Ok(view) => view,
        Err(failure) => return error(format!("failed to load messages: {failure}")),
    };
    let mut extra: Vec<(&str, Value)> = Vec::new();
    if view.window.is_empty() {
        if let Some(owner) = owning.as_deref().filter(|owner| *owner != session_id) {
            if same_lineage(scope, &session_id, owner) {
                if let Ok(rebound) = scope.store.around(owner, anchor, window) {
                    if !rebound.window.is_empty() {
                        extra.push((
                            "warning",
                            json!(format!(
                                "around_message_id {anchor} lives in {owner} (child of {session_id}); rebound transparently"
                            )),
                        ));
                        meta = scope.store.session(owner).unwrap_or(meta);
                        session_id = owner.to_owned();
                        view = rebound;
                    }
                }
            }
        }
    }
    if view.window.is_empty() {
        return error(format!(
            "around_message_id {anchor} not in session_id {session_id}"
        ));
    }
    let mut payload = vec![
        ("mode", json!("scroll")),
        ("session_id", json!(session_id)),
        ("around_message_id", json!(anchor)),
        ("session_meta", meta_block(&meta)),
        ("window", json!(window)),
        (
            "messages",
            Value::Array(view.window.iter().map(|m| shape_message(m, Some(anchor), None, false)).collect()),
        ),
        ("messages_before", json!(view.before)),
        ("messages_after", json!(view.after)),
        (
            "hint",
            json!("Scroll forward: re-call with around_message_id = the LAST message's id; backward: the FIRST message's id (the boundary message repeats as an orientation marker). messages_before/messages_after < window means you've hit that end of the session."),
        ),
    ];
    payload.extend(extra);
    ok(payload)
}

// ---- browse ----

fn list_recent(scope: &Scope<'_>, limit: usize) -> Value {
    let sessions = match scope.store.recent_sessions(limit + 15, &HIDDEN_SOURCES) {
        Ok(sessions) => sessions,
        Err(failure) => return error(format!("Failed to list recent sessions: {failure}")),
    };
    let (root, has_compression) = scope
        .current_session
        .map_or((String::new(), false), |current| {
            resolve_to_parent(scope, current)
        });
    let mut hidden: Vec<&str> = scope.current_session.into_iter().collect();
    if has_compression && !root.is_empty() {
        hidden.push(&root);
    }
    let results: Vec<Value> = sessions
        .iter()
        .filter(|s| !hidden.contains(&s.session_id.as_str()))
        .take(limit)
        .map(|s| {
            let title = redact_derived_text(s.title.as_deref().unwrap_or(""));
            json!({
                "session_id": s.session_id,
                "link": session_link(scope, &s.session_id),
                "title": if title.is_empty() { Value::Null } else { json!(title) },
                "source": s.source,
                "started_at": s.started_at,
                "last_active": s.last_active,
                "message_count": s.message_count,
                "preview": derived_preview(&s.preview),
            })
        })
        .collect();
    let count = results.len();
    ok(vec![
        ("mode", json!("browse")),
        ("results", Value::Array(results)),
        ("count", json!(count)),
        (
            "message",
            json!(format!(
                "Showing {count} most recent sessions. Pass a query= to search, or session_id+around_message_id to scroll."
            )),
        ),
    ])
}

/// A cut at the preview limit can leave a fixed-length key too short for its pattern: drop
/// the cut word before redacting.
fn derived_preview(preview: &str) -> String {
    let mut text = preview.to_owned();
    if text.chars().count() == PREVIEW_MAX_CHARS + 3 && text.ends_with("...") {
        let body: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
        let kept = body.trim_end_matches(|c: char| !c.is_whitespace());
        text = format!("{kept}...");
    }
    redact_derived_text(&text)
}

// ---- discovery ----

struct Discover<'a> {
    query: &'a str,
    roles: Vec<String>,
    limit: usize,
    sort: Option<&'a str>,
    full: bool,
    after: Option<i64>,
    before: Option<i64>,
    exclude: Vec<String>,
}

const LINK_HINT: &str = "When referring the user to a session, write its `link` value verbatim inline mid-sentence (it renders as a titled link) — never as markdown, in backticks, on its own line, or next to the title/id/date. To read more around a compact result, scroll: session_search(session_id=..., around_message_id=match_message_id).";

fn discover_payload(
    scope: &Scope<'_>,
    query: &str,
    detail: &str,
    results: Vec<Value>,
    extra: Vec<(&str, Value)>,
) -> Value {
    let count = results.len();
    let mut payload = vec![
        ("mode", json!("discover")),
        ("query", json!(query)),
        ("detail", json!(detail)),
        ("results", Value::Array(results)),
        ("count", json!(count)),
    ];
    payload.extend(extra);
    if let Some(status) = scope.store.rebuild_status() {
        payload.push((
            "index_rebuild",
            json!({
                "percent": status.percent,
                "note": format!(
                    "The search index is rebuilding in the background ({}% done, {} of {} messages). Results from older messages may be incomplete until it finishes.",
                    status.percent, group_thousands(status.indexed), group_thousands(status.total)
                ),
            }),
        ));
    }
    ok(payload)
}

fn group_thousands(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut out = String::new();
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if value < 0 {
        format!("-{out}")
    } else {
        out
    }
}

fn bookend(rows: &[StoredMessage]) -> Vec<Value> {
    rows.iter()
        .filter(|m| !is_compaction_summary(&m.content))
        .map(|m| shape_message(m, None, Some(1200), true))
        .collect()
}

static SNIPPET_MARK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)>>>(.*?)<<<").expect("valid pattern"));

/// Rebuild the hit snippet from the redacted anchor message: the FTS row contributes only its
/// highlighted terms, because a marker inside a token breaks the vendor-prefix patterns.
fn derived_snippet(fts_snippet: &str, anchor: Option<&StoredMessage>) -> String {
    let Some(anchor) = anchor else {
        return String::new();
    };
    let mut terms: Vec<String> = Vec::new();
    for caps in SNIPPET_MARK.captures_iter(fts_snippet) {
        let term = caps[1].to_owned();
        if !terms.contains(&term) && !term.trim().is_empty() {
            terms.push(term);
        }
    }
    let mut sources = vec![content_text(&redact_value(&anchor.content))];
    if !is_falsy(&anchor.tool_calls) {
        let calls = redact_tool_calls(&anchor.tool_calls);
        sources.push(match &calls {
            Value::String(text) => text.clone(),
            other => py_json_dumps(other),
        });
    }
    let probes: Vec<String> = if terms.is_empty() {
        vec![fts_snippet
            .trim_matches(|c| c == '.' || c == ' ')
            .chars()
            .take(20)
            .collect()]
    } else {
        terms.clone()
    };
    let mut text = sources[0].clone();
    let mut position: Option<usize> = None;
    for candidate in &sources {
        let lowered: Vec<char> = candidate.to_lowercase().chars().collect();
        let found = probes
            .iter()
            .filter(|probe| !probe.is_empty())
            .filter_map(|probe| {
                find_chars(&lowered, &probe.to_lowercase().chars().collect::<Vec<_>>())
            })
            .min();
        if let Some(found) = found {
            text = candidate.clone();
            position = Some(found);
            break;
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let start = position.map_or(0, |pos| pos.saturating_sub(SNIPPET_BEFORE));
    let end = chars.len().min(position.unwrap_or(0) + SNIPPET_AFTER);
    let window: String = chars[start.min(chars.len())..end.max(start.min(chars.len()))]
        .iter()
        .collect();
    let window = if terms.is_empty() {
        window
    } else {
        highlight(&window, &terms)
    };
    format!(
        "{}{}{}",
        if start > 0 { "..." } else { "" },
        window,
        if end < chars.len() { "..." } else { "" }
    )
}

fn find_chars(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| haystack[i..i + needle.len()] == *needle)
}

fn highlight(window: &str, terms: &[String]) -> String {
    let mut sorted: Vec<&String> = terms.iter().collect();
    sorted.sort_by_key(|term| std::cmp::Reverse(term.chars().count()));
    let pattern = sorted
        .iter()
        .map(|term| regex::escape(term))
        .collect::<Vec<_>>()
        .join("|");
    match Regex::new(&format!("(?i){pattern}")) {
        Ok(regex) => regex
            .replace_all(window, |caps: &regex::Captures<'_>| {
                format!(">>>{}<<<", &caps[0])
            })
            .into_owned(),
        Err(_) => window.to_owned(),
    }
}

/// `json.dumps(value, ensure_ascii=False)` with Python's default separators.
fn py_json_dumps(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(py_json_dumps)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| format!(
                    "{}: {}",
                    Value::String(key.clone()),
                    py_json_dumps(item)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

fn discovery_entry(lineage_root: &str, mut fields: Map<String, Value>) -> Value {
    let order = [
        "session_id",
        "when",
        "source",
        "model",
        "title",
        "matched_role",
        "match_message_id",
        "snippet",
        "bookend_start",
        "messages",
        "bookend_end",
        "messages_before",
        "messages_after",
        "detail",
    ];
    let mut entry = Map::new();
    for key in order {
        entry.insert(key.into(), fields.remove(key).unwrap_or(Value::Null));
    }
    if !lineage_root.is_empty() && Some(lineage_root) != entry["session_id"].as_str() {
        entry.insert("parent_session_id".into(), json!(lineage_root));
    }
    Value::Object(entry)
}

fn fields(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// A discovery-shaped result when the query is a session title.
fn title_match(
    scope: &Scope<'_>,
    query: &str,
    current_root: Option<&str>,
) -> Option<(Value, String)> {
    let title_query = query.trim().trim_matches(['`', '\'', '"']);
    if title_query.is_empty() {
        return None;
    }
    let session_id = scope.store.resolve_by_title(title_query)?;
    let lineage_root = resolve_lineage(scope, &session_id);
    if current_root.is_some_and(|root| root == lineage_root)
        && !left_live_context(scope, &session_id)
    {
        return None;
    }
    let meta = scope
        .store
        .session(&session_id)
        .or_else(|| scope.store.session(&lineage_root));
    if meta
        .as_ref()
        .is_some_and(|m| HIDDEN_SOURCES.contains(&m.source.as_str()))
    {
        return None;
    }
    let messages = scope.store.transcript(&session_id).unwrap_or_default();
    let anchor_id = messages.first().map(|m| m.id);
    let view = anchor_id.and_then(|id| scope.store.anchored_view(&session_id, id, 5, 3).ok());
    let title = redact_derived_text(
        meta.as_ref()
            .and_then(|m| m.title.as_deref())
            .filter(|t| !t.is_empty())
            .unwrap_or(title_query),
    );
    let shape = |rows: Option<&Vec<StoredMessage>>,
                 fallback: &[StoredMessage],
                 anchor: Option<i64>,
                 cap: usize|
     -> Vec<Value> {
        let rows: &[StoredMessage] = match rows {
            Some(rows) if !rows.is_empty() => rows,
            _ => fallback,
        };
        rows.iter()
            .map(|m| shape_message(m, anchor, Some(cap), true))
            .collect()
    };
    let head: Vec<StoredMessage> = messages.iter().take(3).cloned().collect();
    let first_five: Vec<StoredMessage> = messages.iter().take(5).cloned().collect();
    let last: Vec<StoredMessage> = messages
        .iter()
        .skip(messages.len().saturating_sub(3))
        .cloned()
        .collect();
    let entry = discovery_entry(
        &lineage_root,
        fields(vec![
            ("session_id", json!(session_id)),
            (
                "when",
                json!(format_timestamp(
                    meta.as_ref().and_then(|m| m.started_at.as_ref())
                )),
            ),
            (
                "source",
                json!(meta.as_ref().map_or("unknown", |m| m.source.as_str())),
            ),
            (
                "model",
                json!(meta
                    .as_ref()
                    .and_then(|m| m.model.clone())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "unknown".into())),
            ),
            ("title", json!(title)),
            ("matched_role", json!("session_title")),
            ("match_message_id", json!(anchor_id)),
            ("snippet", json!(format!("Session title matched: {title}"))),
            (
                "bookend_start",
                Value::Array(shape(
                    view.as_ref().map(|v| &v.bookend_start),
                    &head,
                    None,
                    1200,
                )),
            ),
            (
                "messages",
                Value::Array(shape(
                    view.as_ref().map(|v| &v.window),
                    &first_five,
                    anchor_id,
                    4000,
                )),
            ),
            (
                "bookend_end",
                Value::Array(shape(
                    view.as_ref().map(|v| &v.bookend_end),
                    &last,
                    None,
                    1200,
                )),
            ),
            (
                "messages_before",
                json!(view.as_ref().map_or(0, |v| v.before)),
            ),
            (
                "messages_after",
                json!(view
                    .as_ref()
                    .map_or(messages.len().saturating_sub(5), |v| v.after)),
            ),
            ("detail", json!("full")),
        ]),
    );
    Some((entry, lineage_root))
}

fn hydrate_hit(scope: &Scope<'_>, lineage_root: &str, hit: &Hit, full: bool) -> Option<Value> {
    let hit_session = if hit.session_id.is_empty() {
        lineage_root
    } else {
        hit.session_id.as_str()
    };
    let view = scope
        .store
        .anchored_view(hit_session, hit.message_id, 5, 3)
        .ok()?;
    let meta = scope
        .store
        .session(hit_session)
        .or_else(|| scope.store.session(lineage_root));
    let anchor = view.window.iter().find(|m| m.id == hit.message_id);
    let title = redact_derived_text(meta.as_ref().and_then(|m| m.title.as_deref()).unwrap_or(""));
    let started = meta
        .as_ref()
        .and_then(|m| {
            m.started_at
                .as_ref()
                .filter(|s| !matches!(s, Stamp::Number(n) if *n == 0.0))
        })
        .or(hit.session_started.as_ref());
    let source = meta
        .as_ref()
        .map(|m| m.source.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| hit.source.clone());
    let model = meta
        .as_ref()
        .and_then(|m| m.model.clone())
        .filter(|m| !m.is_empty())
        .or_else(|| hit.model.clone().filter(|m| !m.is_empty()))
        .unwrap_or_else(|| "unknown".into());
    let window_rows: Vec<Value> = view
        .window
        .iter()
        .filter(|m| full || m.id == hit.message_id)
        .map(|m| shape_message(m, Some(hit.message_id), Some(4000), true))
        .collect();
    Some(discovery_entry(
        lineage_root,
        fields(vec![
            ("session_id", json!(hit_session)),
            ("when", json!(format_timestamp(started))),
            ("source", json!(source)),
            ("model", json!(model)),
            (
                "title",
                if title.is_empty() {
                    Value::Null
                } else {
                    json!(title)
                },
            ),
            ("matched_role", json!(hit.role)),
            ("match_message_id", json!(hit.message_id)),
            ("snippet", json!(derived_snippet(&hit.snippet, anchor))),
            (
                "bookend_start",
                Value::Array(if full {
                    bookend(&view.bookend_start)
                } else {
                    Vec::new()
                }),
            ),
            ("messages", Value::Array(window_rows)),
            (
                "bookend_end",
                Value::Array(if full {
                    bookend(&view.bookend_end)
                } else {
                    Vec::new()
                }),
            ),
            ("messages_before", json!(view.before)),
            ("messages_after", json!(view.after)),
            ("detail", json!(if full { "full" } else { "compact" })),
        ]),
    ))
}

fn discover(scope: &Scope<'_>, request: &Discover<'_>, now: i64) -> Value {
    let detail = if request.full { "full" } else { "adaptive" };
    let current_root = scope.current_session.map(|id| resolve_lineage(scope, id));
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for id in &request.exclude {
        excluded.insert(id.clone());
        let root = resolve_lineage(scope, id);
        excluded.insert(if root.is_empty() { id.clone() } else { root });
    }
    let mut title_result = title_match(scope, request.query, current_root.as_deref());
    // The title match skips the SQL time bound, so it is re-checked here.
    if let Some((entry, root)) = &title_result {
        let sid = entry["session_id"].as_str().unwrap_or("");
        let meta = scope
            .store
            .session(root)
            .or_else(|| scope.store.session(sid));
        let started = started_ts(meta.as_ref().and_then(|m| m.started_at.as_ref()), now);
        if excluded.contains(sid)
            || excluded.contains(root)
            || !in_time_window(started, request.after, request.before)
        {
            title_result = None;
        }
    }
    let roles = request.roles.clone();
    let mut raw = match scope.store.search(&SearchRequest {
        query: request.query,
        role_filter: &roles,
        exclude_sources: &HIDDEN_SOURCES,
        limit: DISCOVER_SCAN_LIMIT,
        sort: request.sort,
        after_ts: request.after,
        before_ts: request.before,
    }) {
        Ok(rows) => rows,
        Err(failure) => return error(format!("Search failed: {failure}")),
    };
    // Cron rows rank below interactive ones before dedup; the stable sort keeps BM25 order.
    raw.sort_by_key(|hit| DEMOTED_SOURCES.contains(&hit.source.as_str()));
    if raw.is_empty() && title_result.is_none() {
        return discover_payload(
            scope,
            request.query,
            detail,
            Vec::new(),
            vec![(
                "message",
                json!("No matching sessions found. FTS5 ANDs all terms by default — broaden with OR (`alpha OR beta`), exact-match with quoted phrases, exclude with NOT, or prefix-match with `deploy*`."),
            )],
        );
    }
    // Lineage root -> first surviving row; `None` marks the lineage the title match holds.
    let mut seen: Vec<(String, Option<Hit>)> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    if let Some((entry, root)) = title_result {
        results.push(entry);
        seen.push((root, None));
    }
    let mut hidden_rows = 0_usize;
    let mut hidden_sessions: Vec<Value> = Vec::new();
    let mut hidden_ids: Vec<String> = Vec::new();
    for hit in raw {
        if seen.len() >= request.limit {
            break;
        }
        let resolved = resolve_lineage(scope, &hit.session_id);
        if excluded.contains(&hit.session_id) || excluded.contains(&resolved) {
            continue;
        }
        let compacted = scope
            .store
            .storage_state(hit.message_id)
            .is_some_and(|(_, visibility)| visibility == Visibility::CompactionArchived);
        if current_root.as_deref() == Some(resolved.as_str())
            && !(left_live_context(scope, &hit.session_id) || compacted)
        {
            // An unended same-lineage row is dropped with no signal, so the agent would
            // conclude the conversation never existed: disclose it.
            if scope.current_session != Some(hit.session_id.as_str()) {
                hidden_rows += 1;
                if !hidden_ids.contains(&hit.session_id) {
                    hidden_ids.push(hit.session_id.clone());
                    let meta = scope.store.session(&hit.session_id);
                    hidden_sessions.push(json!({
                        "session_id": hit.session_id,
                        "end_reason": meta.as_ref().and_then(|m| m.end_reason.clone()),
                        "last_activity": format_timestamp(meta.as_ref().and_then(|m| m.last_activity_at.as_ref())),
                    }));
                }
            }
            continue;
        }
        if scope.current_session == Some(hit.session_id.as_str()) && !compacted {
            continue;
        }
        if !seen.iter().any(|(root, _)| *root == resolved) {
            seen.push((resolved, Some(hit)));
        }
    }
    for (root, hit) in &seen {
        let Some(hit) = hit else { continue };
        // Adaptive: only the top-ranked result is fully hydrated.
        let full = request.full || results.is_empty();
        if let Some(entry) = hydrate_hit(scope, root, hit, full) {
            results.push(entry);
        }
    }
    for entry in &mut results {
        let link = session_link(scope, entry["session_id"].as_str().unwrap_or(""));
        entry["link"] = json!(link);
    }
    let mut extra = vec![("sessions_searched", json!(seen.len()))];
    if hidden_rows > 0 {
        let sessions = hidden_sessions.len();
        extra.push((
            "hidden_same_lineage",
            json!({
                "count": hidden_rows,
                "sessions": hidden_sessions,
                "note": format!("{hidden_rows} matching message(s) in {sessions} same-lineage session(s) were hidden because they are still treated as live context (unended). This can be a gateway session whose end-write was lost. To read one anyway, pass only its session_id: session_search(session_id=...)."),
            }),
        ));
    }
    extra.push(("link_hint", json!(LINK_HINT)));
    discover_payload(scope, request.query, detail, results, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_integers_follow_python() {
        assert_eq!(py_int(&json!("7")), Some(7));
        assert_eq!(py_int(&json!(3.9)), Some(3));
        assert_eq!(py_int(&json!(true)), Some(1));
        assert_eq!(py_int(&json!("x")), None);
        assert_eq!(clamp_int(Some(&json!(99)), 3, 1, 10), 10);
        assert_eq!(clamp_int(None, 3, 1, 10), 3);
        assert_eq!(clamp_int(Some(&json!("0")), 5, 1, 20), 1);
    }

    #[test]
    fn time_bounds_accept_iso_dates_and_relative_durations() {
        let now = 1_800_000_000;
        assert_eq!(
            parse_bound(Some(&json!("7d")), now),
            Ok(Some(now - 7 * 86400))
        );
        assert_eq!(
            parse_bound(Some(&json!("24H")), now),
            Ok(Some(now - 24 * 3600))
        );
        assert_eq!(
            parse_bound(Some(&json!("2026-07-01")), now),
            Ok(Some(1_782_864_000))
        );
        assert_eq!(
            parse_bound(Some(&json!("2026-07-01T12:00:00+02:00")), now),
            Ok(Some(1_782_900_000))
        );
        assert_eq!(parse_bound(Some(&json!("  ")), now), Ok(None));
        assert_eq!(parse_bound(None, now), Ok(None));
        let message = parse_bound(Some(&json!("soon")), now).unwrap_err();
        assert!(message.starts_with("invalid time bound: 'soon' (expected ISO date/datetime"));
    }

    #[test]
    fn a_time_window_fails_an_unknown_start() {
        assert!(in_time_window(None, None, None));
        assert!(!in_time_window(None, Some(1), None));
        assert!(in_time_window(Some(5), Some(5), Some(6)));
        assert!(!in_time_window(Some(6), Some(5), Some(6)));
    }

    #[test]
    fn exclusions_are_trimmed_deduplicated_and_capped() {
        let ids: Vec<Value> = (0..30).map(|n| json!(format!(" s{} ", n % 25))).collect();
        let out = normalize_exclude(Some(&Value::Array(ids)));
        assert_eq!(out.len(), 20);
        assert_eq!(out[0], "s0");
        assert_eq!(normalize_exclude(Some(&json!("one"))), vec!["one"]);
        assert!(normalize_exclude(Some(&json!(5))).is_empty());
    }

    #[test]
    fn python_reprs_pick_the_quote_that_needs_no_escape() {
        assert_eq!(py_repr("a'b"), "\"a'b\"");
        assert_eq!(py_repr("plain"), "'plain'");
        assert_eq!(py_repr("a\nb"), "'a\\nb'");
    }

    #[test]
    fn profile_names_are_validated_before_any_path_is_built() {
        let home = Path::new("/nonexistent/home");
        for name in ["../x", "a/b", "UPPER ok?", "-lead"] {
            let failure = open_profile(home, name).err().expect("refused");
            assert!(
                failure.contains("is not a valid profile name"),
                "{name}: {failure}"
            );
        }
        assert!(open_profile(home, "root")
            .err()
            .unwrap()
            .contains("is reserved"));
        assert!(open_profile(home, "missing")
            .err()
            .unwrap()
            .contains("does not exist"));
    }

    #[test]
    fn data_urls_are_replaced_and_other_strings_redacted() {
        let value = json!(["data:image/png;base64,AAAA", {"text": "plain"}]);
        let out = redact_value(&value);
        assert_eq!(out[0], DATA_URL_PLACEHOLDER);
        assert_eq!(out[1]["text"], "plain");
    }

    #[test]
    fn snippets_are_rebuilt_from_the_redacted_message_around_the_match() {
        let message = StoredMessage {
            id: 1,
            role: "user".into(),
            content: json!(format!("{} needle {}", "a".repeat(150), "b".repeat(300))),
            timestamp: 1.0,
            tool_calls: Value::Null,
            tool_name: None,
            tool_call_id: None,
        };
        let snippet = derived_snippet("x >>>needle<<< y", Some(&message));
        assert!(snippet.starts_with("..."));
        assert!(snippet.contains(">>>needle<<<"));
        assert!(snippet.ends_with("..."));
        assert_eq!(derived_snippet("x", None), "");
    }
}
