//! Read-only queries behind `session_search`: ranked message search, anchored windows,
//! lineage walks and the bounded list of recent sessions.
//!
//! Follows `persistence/sqlite/conversation/_store/{search,messages,sessions}.py` of the
//! reference. The queries run against a read-only handle, so they are safe on a live store.
//! Not ported: the corruption fail-open paths (a damaged index answers with an error),
//! and skill-scaffold handling in the browse preview (see `PREVIEW_HEAD_CHARS`).

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::types::Value as Sql;
use rusqlite::{params_from_iter, Connection, OptionalExtension, Row};
use serde_json::Value;

use super::{
    open_read_only, read_content, read_schema_version, StorageError, LEGACY_SCHEMA_VERSION,
    SCHEMA_VERSION,
};

const PREVIEW_HEAD_CHARS: usize = 63;
const MAX_FTS_QUERY_CHARS: usize = 2048;
const EPOCH_MAX: f64 = 4_200_000_000.0;
/// Session end reasons that start a fresh conversation without carrying the transcript.
pub const BOUNDARY_END_REASONS: [&str; 7] = [
    "session_reset",
    "session_switch",
    "idle",
    "daily",
    "suspended",
    "resume_pending_expired",
    "new_session",
];
const RESET_END_REASONS_SQL: &str =
    "'session_reset', 'session_switch', 'idle', 'daily', 'suspended', 'resume_pending_expired'";

/// A stored timestamp: a number, or the text of an old row.
#[derive(Clone, Debug, PartialEq)]
pub enum Stamp {
    Number(f64),
    Text(String),
}

fn stamp(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<Stamp>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(column)? {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(Stamp::Number(value as f64)),
        ValueRef::Real(value) => Some(Stamp::Number(value)),
        ValueRef::Text(bytes) => Some(Stamp::Text(String::from_utf8_lossy(bytes).into_owned())),
        ValueRef::Blob(_) => None,
    })
}

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub source: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub parent_session_id: Option<String>,
    pub started_at: Option<Stamp>,
    pub last_activity_at: Option<Stamp>,
    pub end_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Active,
    CompactionArchived,
    Rewound,
}

impl Visibility {
    fn of(active: bool, compacted: bool) -> Self {
        if active {
            Self::Active
        } else if compacted {
            Self::CompactionArchived
        } else {
            Self::Rewound
        }
    }
}

#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub id: i64,
    pub role: String,
    pub content: Value,
    pub timestamp: f64,
    pub tool_calls: Value,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub message_id: i64,
    pub session_id: String,
    pub role: String,
    pub snippet: String,
    pub source: String,
    pub model: Option<String>,
    pub session_started: Option<Stamp>,
}

pub struct AnchoredWindow {
    pub window: Vec<StoredMessage>,
    pub before: usize,
    pub after: usize,
}

pub struct AnchoredView {
    pub window: Vec<StoredMessage>,
    pub before: usize,
    pub after: usize,
    pub bookend_start: Vec<StoredMessage>,
    pub bookend_end: Vec<StoredMessage>,
}

#[derive(Debug)]
pub struct RecentSession {
    pub session_id: String,
    pub title: Option<String>,
    pub source: String,
    pub started_at: f64,
    pub last_active: Option<f64>,
    pub message_count: i64,
    pub preview: String,
}

pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub role_filter: &'a [String],
    pub exclude_sources: &'a [&'a str],
    pub limit: usize,
    pub sort: Option<&'a str>,
    pub after_ts: Option<i64>,
    pub before_ts: Option<i64>,
}

pub struct RebuildStatus {
    pub percent: i64,
    pub indexed: i64,
    pub total: i64,
}

pub struct RecallStore {
    connection: Connection,
}

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionMeta> {
    Ok(SessionMeta {
        id: row.get(0)?,
        source: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        model: row.get(2)?,
        title: row.get(3)?,
        parent_session_id: row.get(4)?,
        started_at: stamp(row, 5)?,
        last_activity_at: stamp(row, 6)?,
        end_reason: row.get(7)?,
    })
}

const SESSION_COLUMNS: &str =
    "id, source, model, title, parent_session_id, started_at, last_activity_at, end_reason";

fn message_row(row: &Row<'_>) -> rusqlite::Result<StoredMessage> {
    let content = read_content(row, 2)?;
    let tool_calls = row
        .get::<_, Option<String>>(4)?
        .filter(|text| !text.is_empty())
        .map(|text| serde_json::from_str(&text).unwrap_or_else(|_| Value::Array(Vec::new())))
        .unwrap_or(Value::Null);
    Ok(StoredMessage {
        id: row.get(0)?,
        role: row.get(1)?,
        content,
        timestamp: row.get(3)?,
        tool_calls,
        tool_name: row.get(5)?,
        tool_call_id: row.get(6)?,
    })
}

const MESSAGE_COLUMNS: &str = "id, role, content, timestamp, tool_calls, tool_name, tool_call_id";

fn is_cjk(c: char) -> bool {
    matches!(u32::from(c),
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x3000..=0x303F
        | 0x3040..=0x309F | 0x30A0..=0x30FF | 0xAC00..=0xD7AF)
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(is_cjk)
}

fn count_cjk(text: &str) -> usize {
    text.chars().filter(|c| is_cjk(*c)).count()
}

fn is_operator(token: &str) -> bool {
    matches!(token.to_uppercase().as_str(), "AND" | "OR" | "NOT")
}

static QUOTED_PHRASE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*""#).expect("valid pattern"));
static LIKE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]+"|\S+"#).expect("valid pattern"));
static STARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*+").expect("valid pattern"));
static LEADING_STAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|\s)\*").expect("valid pattern"));
static LEADING_OPERATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(AND|OR|NOT)\b\s*").expect("valid pattern"));
static TRAILING_OPERATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+(AND|OR|NOT)\s*$").expect("valid pattern"));
static COMPOUND_TERM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\w+(?:[._-]\w+)+)\b").expect("valid pattern"));
const FTS_SPECIAL: &str = "+{}():\"^@/#&|~[]<>,;!?$=\\'";

/// Clean user input for FTS5 `MATCH`: keep balanced quoted phrases, drop special characters
/// and dangling operators, quote dotted and hyphenated terms.
fn sanitize_fts_query(query: &str) -> String {
    let query: String = query.chars().take(MAX_FTS_QUERY_CHARS).collect();
    let mut held: Vec<String> = Vec::new();
    let replaced = QUOTED_PHRASE.replace_all(&query, |caps: &regex::Captures<'_>| {
        held.push(caps[0].to_owned());
        format!("\x00Q{}\x00", held.len() - 1)
    });
    let mut text: String = replaced
        .replace('"', " ")
        .chars()
        .map(|c| if FTS_SPECIAL.contains(c) { ' ' } else { c })
        .collect();
    if text.contains('%') && !contains_cjk(&text) {
        text = text.replace('%', " ");
    }
    let text = STARS.replace_all(&text, "*").into_owned();
    let text = LEADING_STAR.replace_all(&text, "$1").into_owned();
    let text = LEADING_OPERATOR.replace(text.trim(), "").into_owned();
    let text = TRAILING_OPERATOR.replace(text.trim(), "").into_owned();
    let mut text = COMPOUND_TERM.replace_all(&text, "\"$1\"").into_owned();
    for (index, quoted) in held.iter().enumerate() {
        text = text.replace(&format!("\x00Q{index}\x00"), quoted);
    }
    text.trim().to_owned()
}

fn quote_fts_tokens(query: &str) -> String {
    query
        .split_whitespace()
        .map(|token| {
            if is_operator(token) {
                token.to_owned()
            } else {
                format!("\"{}\"", token.replace('"', "\"\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_cjk_wildcards(query: &str) -> String {
    if !query.contains('*') {
        return query.to_owned();
    }
    let stripped = query
        .split_whitespace()
        .map(|token| {
            if is_operator(token) {
                token
            } else {
                let trimmed = token.trim_end_matches('*');
                if trimmed.is_empty() {
                    token
                } else {
                    trimmed
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if stripped.is_empty() {
        query.to_owned()
    } else {
        stripped
    }
}

fn non_operator_tokens(query: &str) -> Vec<&str> {
    query
        .split_whitespace()
        .filter(|token| !is_operator(token))
        .collect()
}

fn trigram_eligible(query: &str) -> bool {
    let tokens = non_operator_tokens(query.trim_matches('"').trim());
    !tokens.is_empty() && tokens.iter().all(|token| token.chars().count() >= 3)
}

fn trigram_route_ok(query: &str, available: bool) -> bool {
    available
        && count_cjk(query) >= 3
        && !non_operator_tokens(query)
            .iter()
            .any(|token| contains_cjk(token) && count_cjk(token) < 3)
}

/// The any-term form of an implicit-AND query; `None` when it does not apply.
fn or_relaxed(query: &str) -> Option<String> {
    let mut units: Vec<&str> = Vec::new();
    for token in LIKE_TOKEN.find_iter(query).map(|m| m.as_str()) {
        let upper = token.to_uppercase();
        if upper == "OR" || upper == "NOT" {
            return None;
        }
        if upper != "AND" {
            units.push(token);
        }
    }
    (units.len() >= 2).then(|| units.join(" OR "))
}

fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

const LIKE_COLUMNS: &str = "(COALESCE(m.content, '') LIKE ? ESCAPE '\\' OR COALESCE(m.tool_name, '') LIKE ? ESCAPE '\\' OR COALESCE(m.tool_calls, '') LIKE ? ESCAPE '\\')";
const LIKE_ANY_COLUMN: &str = "(m.content LIKE ? ESCAPE '\\' OR m.tool_name LIKE ? ESCAPE '\\' OR m.tool_calls LIKE ? ESCAPE '\\')";

fn like_params(term: &str) -> [Sql; 3] {
    let pattern = format!("%{}%", escape_like(term));
    [
        Sql::Text(pattern.clone()),
        Sql::Text(pattern.clone()),
        Sql::Text(pattern),
    ]
}

/// The supported boolean subset as LIKE predicates: terms in an OR group are ANDed and
/// `NOT` negates the next term. Returns the predicate, its parameters and the snippet term.
fn compile_like_query(query: &str) -> (String, Vec<Sql>, Option<String>) {
    let mut groups: Vec<Vec<(String, bool)>> = vec![Vec::new()];
    let mut negate = false;
    for token in LIKE_TOKEN.find_iter(query).map(|m| m.as_str()) {
        match token.to_uppercase().as_str() {
            "OR" => {
                if !groups.last().is_some_and(Vec::is_empty) {
                    groups.push(Vec::new());
                }
                negate = false;
                continue;
            }
            "AND" | "NEAR" => continue,
            "NOT" => {
                negate = true;
                continue;
            }
            _ => {}
        }
        let term = token.trim_matches('"').trim_matches('*').trim();
        if !term.is_empty() {
            groups
                .last_mut()
                .expect("one group")
                .push((term.to_owned(), negate));
            negate = false;
        }
    }
    let mut compiled = Vec::new();
    let mut params = Vec::new();
    let mut snippet_term = None;
    for group in groups {
        if group.is_empty() || group.iter().all(|(_, negated)| *negated) {
            continue;
        }
        let mut clauses = Vec::new();
        for (term, negated) in group {
            clauses.push(if negated {
                format!("NOT {LIKE_COLUMNS}")
            } else {
                LIKE_COLUMNS.to_owned()
            });
            params.extend(like_params(&term));
            if snippet_term.is_none() && !negated {
                snippet_term = Some(term);
            }
        }
        compiled.push(format!("({})", clauses.join(" AND ")));
    }
    (compiled.join(" OR "), params, snippet_term)
}

struct Filters<'a> {
    role_filter: &'a [String],
    exclude_sources: &'a [&'a str],
    after_ts: Option<i64>,
    before_ts: Option<i64>,
}

fn filter_clauses(where_: &mut Vec<String>, params: &mut Vec<Sql>, filters: &Filters<'_>) {
    where_.push("(m.active = 1 OR m.compacted = 1)".into());
    where_.push("COALESCE(m.display_kind, '') <> 'hidden'".into());
    if !filters.exclude_sources.is_empty() {
        let marks = vec!["?"; filters.exclude_sources.len()].join(",");
        where_.push(format!("s.source NOT IN ({marks})"));
        params.extend(
            filters
                .exclude_sources
                .iter()
                .map(|s| Sql::Text((*s).to_owned())),
        );
    }
    if !filters.role_filter.is_empty() {
        let marks = vec!["?"; filters.role_filter.len()].join(",");
        where_.push(format!("m.role IN ({marks})"));
        params.extend(filters.role_filter.iter().map(|r| Sql::Text(r.clone())));
    }
    if let Some(after) = filters.after_ts {
        where_.push("s.started_at >= ?".into());
        params.push(Sql::Integer(after));
    }
    if let Some(before) = filters.before_ts {
        where_.push("s.started_at < ?".into());
        params.push(Sql::Integer(before));
    }
}

fn hit_row(row: &Row<'_>) -> rusqlite::Result<Hit> {
    Ok(Hit {
        message_id: row.get(0)?,
        session_id: row.get(1)?,
        role: row.get(2)?,
        snippet: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        model: row.get(5)?,
        source: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        session_started: stamp(row, 6)?,
    })
}

const HIT_TAIL: &str = "s.source, s.model, s.started_at";

impl RecallStore {
    pub fn open(profile: &Path) -> Result<Self, StorageError> {
        let connection = open_read_only(profile)?;
        match read_schema_version(&connection)? {
            LEGACY_SCHEMA_VERSION | SCHEMA_VERSION => Ok(Self { connection }),
            version => Err(StorageError::UnsupportedSchema(version)),
        }
    }

    fn table_exists(&self, name: &str) -> bool {
        self.connection
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE name = ?1 AND type = 'table'",
                [name],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.connection
            .query_row(
                "SELECT value FROM state_meta WHERE key = ?1",
                [key],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    pub fn session(&self, id: &str) -> Option<SessionMeta> {
        self.connection
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                [id],
                session_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    /// The title's session, preferring the latest `title #N` continuation.
    pub fn resolve_by_title(&self, title: &str) -> Option<String> {
        let exact: Option<String> = self
            .connection
            .query_row("SELECT id FROM sessions WHERE title = ?1", [title], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten();
        if title == "Bot Chat" {
            return exact;
        }
        let numbered: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM sessions WHERE title LIKE ?1 ESCAPE '\\' ORDER BY started_at DESC",
                [format!("{} #%", escape_like(title))],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        numbered.or(exact)
    }

    /// Live rows of one session in insertion order.
    pub fn transcript(&self, session_id: &str) -> Result<Vec<StoredMessage>, StorageError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND active = 1 ORDER BY id ASC"
        ))?;
        let rows = statement.query_map([session_id], message_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The owning session and visibility of one message.
    pub fn storage_state(&self, message_id: i64) -> Option<(String, Visibility)> {
        if message_id == 0 {
            return None;
        }
        self.connection
            .query_row(
                "SELECT session_id, active, compacted FROM messages WHERE id = ?1",
                [message_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Visibility::of(row.get(1)?, row.get(2)?),
                    ))
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Up to `window` rows either side of the anchor (the anchor included), inactive rows too.
    pub fn around(
        &self,
        session_id: &str,
        anchor: i64,
        window: usize,
    ) -> Result<AnchoredWindow, StorageError> {
        let found = self
            .connection
            .query_row(
                "SELECT 1 FROM messages WHERE id = ?1 AND session_id = ?2 LIMIT 1",
                rusqlite::params![anchor, session_id],
                |_| Ok(()),
            )
            .optional()?;
        if found.is_none() {
            return Ok(AnchoredWindow {
                window: Vec::new(),
                before: 0,
                after: 0,
            });
        }
        let mut before = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND id <= ?2 ORDER BY id DESC LIMIT ?3"
        ))?;
        let before_rows: Vec<StoredMessage> = before
            .query_map(
                rusqlite::params![session_id, anchor, window as i64 + 1],
                message_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        let mut after = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3"
        ))?;
        let after_rows: Vec<StoredMessage> = after
            .query_map(
                rusqlite::params![session_id, anchor, window as i64],
                message_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        let (before_count, after_count) = (before_rows.len().saturating_sub(1), after_rows.len());
        let rows = before_rows.into_iter().rev().chain(after_rows).collect();
        Ok(AnchoredWindow {
            window: rows,
            before: before_count,
            after: after_count,
        })
    }

    /// The anchored window plus the first and last `bookend` non-empty user and assistant
    /// rows outside it; the window keeps only those roles and the anchor.
    pub fn anchored_view(
        &self,
        session_id: &str,
        anchor: i64,
        window: usize,
        bookend: usize,
    ) -> Result<AnchoredView, StorageError> {
        let around = self.around(session_id, anchor, window)?;
        let (Some(first), Some(last)) = (
            around.window.first().map(|r| r.id),
            around.window.last().map(|r| r.id),
        ) else {
            return Ok(AnchoredView {
                window: Vec::new(),
                before: 0,
                after: 0,
                bookend_start: Vec::new(),
                bookend_end: Vec::new(),
            });
        };
        let kept = around
            .window
            .into_iter()
            .filter(|row| row.id == anchor || row.role == "user" || row.role == "assistant")
            .collect();
        let edge = |operator: &str,
                    boundary: i64,
                    order: &str|
         -> Result<Vec<StoredMessage>, StorageError> {
            let mut statement = self.connection.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND id {operator} ?2 \
                 AND role IN ('user', 'assistant') AND length(content) > 0 ORDER BY id {order} LIMIT ?3"
            ))?;
            let rows = statement.query_map(
                rusqlite::params![session_id, boundary, bookend as i64],
                message_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        let (start, mut end) = if bookend > 0 {
            (edge("<", first, "ASC")?, edge(">", last, "DESC")?)
        } else {
            (Vec::new(), Vec::new())
        };
        end.reverse();
        Ok(AnchoredView {
            window: kept,
            before: around.before,
            after: around.after,
            bookend_start: start,
            bookend_end: end,
        })
    }

    pub fn rebuild_status(&self) -> Option<RebuildStatus> {
        let total: i64 = self
            .meta("fts_rebuild_high_water")?
            .parse()
            .ok()
            .filter(|total| *total > 0)?;
        let indexed: i64 = self
            .meta("fts_rebuild_progress")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Some(RebuildStatus {
            percent: (100 * indexed / total).min(100),
            indexed,
            total,
        })
    }

    fn like_rows(
        &self,
        where_: &[String],
        params: Vec<Sql>,
        order_by: &str,
        limit_sql: &str,
    ) -> Result<Vec<Hit>, StorageError> {
        let sql = format!(
            "SELECT m.id, m.session_id, m.role, substr(m.content, max(1, instr(m.content, ?) - 40), 120) AS snippet, {HIT_TAIL} \
             FROM messages m JOIN sessions s ON s.id = m.session_id WHERE {} {order_by} {limit_sql}",
            where_.join(" AND ")
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params), hit_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn like_fallback(
        &self,
        query: &str,
        limit: usize,
        sort: Option<&str>,
        filters: &Filters<'_>,
    ) -> Result<Vec<Hit>, StorageError> {
        let (predicate, mut params, snippet_term) = compile_like_query(query);
        let Some(snippet_term) = snippet_term.filter(|_| !predicate.is_empty()) else {
            return Ok(Vec::new());
        };
        let mut where_ = vec![format!("({predicate})")];
        filter_clauses(&mut where_, &mut params, filters);
        let order = if sort.is_some_and(|s| s.trim().eq_ignore_ascii_case("oldest")) {
            "ASC"
        } else {
            "DESC"
        };
        let mut all = vec![Sql::Text(snippet_term)];
        all.extend(params);
        all.extend([Sql::Integer(limit as i64), Sql::Integer(0)]);
        self.like_rows(
            &where_,
            all,
            &format!("ORDER BY m.timestamp {order}, m.id {order}"),
            "LIMIT ? OFFSET ?",
        )
    }

    /// One `MATCH` against an FTS table; `None` when the query cannot run (tokenizer or syntax).
    fn match_rows(
        &self,
        table: &str,
        match_query: &str,
        order_by: &str,
        limit: usize,
        filters: &Filters<'_>,
    ) -> Option<Vec<Hit>> {
        let mut where_ = vec![format!("{table} MATCH ?")];
        let mut params = vec![Sql::Text(match_query.to_owned())];
        filter_clauses(&mut where_, &mut params, filters);
        params.extend([Sql::Integer(limit as i64), Sql::Integer(0)]);
        let sql = format!(
            "SELECT m.id, m.session_id, m.role, snippet({table}, -1, '>>>', '<<<', '...', 40) AS snippet, {HIT_TAIL} \
             FROM {table} JOIN messages m ON m.id = {table}.rowid JOIN sessions s ON s.id = m.session_id \
             WHERE {} {order_by} LIMIT ? OFFSET ?",
            where_.join(" AND ")
        );
        let mut statement = self.connection.prepare(&sql).ok()?;
        let rows = statement
            .query_map(params_from_iter(params), hit_row)
            .ok()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().ok()
    }

    /// The rows a rebuild has not indexed yet, found by substring scan.
    fn unindexed_gap(&self, query: &str, limit: usize, filters: &Filters<'_>) -> Vec<Hit> {
        let Some(status) = self.rebuild_status() else {
            return Vec::new();
        };
        if limit == 0 {
            return Vec::new();
        }
        let terms: Vec<String> = LIKE_TOKEN
            .find_iter(query)
            .map(|m| {
                m.as_str()
                    .trim_matches('"')
                    .trim_matches('*')
                    .trim()
                    .to_owned()
            })
            .filter(|term| {
                !term.is_empty()
                    && !matches!(term.to_uppercase().as_str(), "AND" | "OR" | "NOT" | "NEAR")
            })
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut where_ = vec!["m.id > ? AND m.id <= ?".to_owned()];
        where_.extend(terms.iter().map(|_| LIKE_ANY_COLUMN.to_owned()));
        let mut params = vec![Sql::Integer(status.indexed), Sql::Integer(status.total)];
        params.extend(terms.iter().flat_map(|term| like_params(term)));
        filter_clauses(&mut where_, &mut params, filters);
        let mut all = vec![Sql::Text(terms[0].clone())];
        all.extend(params);
        all.push(Sql::Integer(limit as i64));
        self.like_rows(&where_, all, "ORDER BY m.timestamp DESC", "LIMIT ?")
            .unwrap_or_default()
    }

    /// Ranked search over the message indexes: keywords, quoted phrases, `AND`/`OR`/`NOT`,
    /// `prefix*`. Falls back to substring scans for CJK, tool output and a stale index,
    /// and relaxes a missed implicit-AND query to any-term.
    pub fn search(&self, request: &SearchRequest<'_>) -> Result<Vec<Hit>, StorageError> {
        if request.query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let query = sanitize_fts_query(request.query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let filters = Filters {
            role_filter: request.role_filter,
            exclude_sources: request.exclude_sources,
            after_ts: request.after_ts,
            before_ts: request.before_ts,
        };
        let sort = request.sort;
        let wants_tool = request.role_filter.iter().any(|role| role == "tool");
        if wants_tool {
            return self.like_fallback(&query, request.limit, sort, &filters);
        }
        let stale = self.meta("fts_stale").is_some();
        if stale {
            return self.search_stale(&query, request.limit, sort, &filters);
        }
        if !self.table_exists("messages_fts") {
            return Ok(Vec::new());
        }
        let order_by = match sort.map(|s| s.trim().to_lowercase()).as_deref() {
            Some("newest") => "ORDER BY m.timestamp DESC, rank",
            Some("oldest") => "ORDER BY m.timestamp ASC, rank",
            _ => "ORDER BY rank",
        };
        let trigram = self.table_exists("messages_fts_trigram");
        let cjk = contains_cjk(&query);
        let mut matches: Vec<Hit> = if cjk {
            self.search_cjk(&query, trigram, order_by, request.limit, &filters)?
        } else {
            match self.match_rows("messages_fts", &query, order_by, request.limit, &filters) {
                Some(rows) => rows,
                None => return Ok(Vec::new()),
            }
        };
        if self.rebuild_status().is_some() && matches.len() < request.limit {
            let gap = self.unindexed_gap(&query, request.limit - matches.len(), &filters);
            for hit in gap {
                if !matches.iter().any(|seen| seen.message_id == hit.message_id) {
                    matches.push(hit);
                }
            }
        }
        // unicode61 puts no boundary between Latin and adjacent CJK; retry on the trigram index.
        if matches.is_empty() && !cjk && trigram && trigram_eligible(&query) {
            let fallback = quote_fts_tokens(query.trim_matches('"').trim());
            if let Some(rows) = self.match_rows(
                "messages_fts_trigram",
                &fallback,
                order_by,
                request.limit,
                &filters,
            ) {
                matches = rows;
            }
        }
        if matches.is_empty() && !cjk {
            if let Some(relaxed) = or_relaxed(&query) {
                if let Some(rows) =
                    self.match_rows("messages_fts", &relaxed, order_by, request.limit, &filters)
                {
                    matches = rows;
                }
            }
        }
        Ok(matches)
    }

    fn search_stale(
        &self,
        query: &str,
        limit: usize,
        sort: Option<&str>,
        filters: &Filters<'_>,
    ) -> Result<Vec<Hit>, StorageError> {
        let matches = self.like_fallback(query, limit, sort, filters)?;
        if !matches.is_empty() || contains_cjk(query) {
            return Ok(matches);
        }
        match or_relaxed(query) {
            Some(relaxed) => self.like_fallback(&relaxed, limit, sort, filters),
            None => Ok(matches),
        }
    }

    fn search_cjk(
        &self,
        query: &str,
        trigram: bool,
        order_by: &str,
        limit: usize,
        filters: &Filters<'_>,
    ) -> Result<Vec<Hit>, StorageError> {
        let raw = strip_cjk_wildcards(query)
            .trim_matches('"')
            .trim()
            .to_owned();
        let match_query = quote_fts_tokens(&raw);
        let unindexed_source = false;
        if trigram_route_ok(&raw, trigram) && !unindexed_source {
            if let Some(rows) = self.match_rows(
                "messages_fts_trigram",
                &match_query,
                order_by,
                limit,
                filters,
            ) {
                return Ok(rows);
            }
        }
        let mut tokens = non_operator_tokens(&raw);
        if tokens.is_empty() {
            tokens = vec![raw.as_str()];
        }
        let mut params: Vec<Sql> = tokens.iter().flat_map(|token| like_params(token)).collect();
        let mut where_ = vec![format!(
            "({})",
            vec![LIKE_ANY_COLUMN; tokens.len()].join(" OR ")
        )];
        filter_clauses(&mut where_, &mut params, filters);
        let mut all = vec![Sql::Text(tokens[0].to_owned())];
        all.extend(params);
        all.extend([Sql::Integer(limit as i64), Sql::Integer(0)]);
        self.like_rows(
            &where_,
            all,
            "ORDER BY m.timestamp DESC",
            "LIMIT ? OFFSET ?",
        )
    }

    /// The most recent browsable sessions: compression chains collapse to their newest tip,
    /// and subagent, archived and hidden sessions are left out.
    pub fn recent_sessions(
        &self,
        limit: usize,
        exclude_sources: &[&str],
    ) -> Result<Vec<RecentSession>, StorageError> {
        let limit = limit.max(1);
        let candidate_limit = limit.saturating_mul(8).clamp(128, 2048).max(limit);
        let lineage_limit = (candidate_limit * 8).clamp(candidate_limit, 8192);
        let delegate = json_marker("s.model_config", "_delegate_from");
        let mut candidate = format!("s.archived = 0 AND s.hidden = 0 AND {delegate} IS NULL");
        let mut params: Vec<Sql> = Vec::new();
        if !exclude_sources.is_empty() {
            candidate.push_str(&format!(
                " AND s.source NOT IN ({})",
                vec!["?"; exclude_sources.len()].join(",")
            ));
            params.extend(exclude_sources.iter().map(|s| Sql::Text((*s).to_owned())));
        }
        let edge = format!(
            "parent.end_reason = 'compression' AND child.parent_session_id = parent.id \
             AND {} IS NULL AND {} IS NULL AND COALESCE(child.source, '') != 'tool'",
            json_marker("child.model_config", "_branched_from"),
            json_marker("child.model_config", "_delegate_from"),
        );
        let activity = freshest("c.cur_id");
        let branch_child = format!(
            "{} IS NOT NULL OR EXISTS (SELECT 1 FROM sessions p WHERE p.id = s.parent_session_id \
             AND p.end_reason = 'branched' AND s.started_at >= p.ended_at)",
            json_marker("s.model_config", "_branched_from")
        );
        let reset_child = format!(
            "{} IS NOT NULL OR EXISTS (SELECT 1 FROM sessions p WHERE p.id = s.parent_session_id \
             AND p.end_reason IN ({RESET_END_REASONS_SQL}) AND s.session_key IS NOT NULL \
             AND s.session_key != '' AND s.session_key = p.session_key)",
            json_marker("s.model_config", "_reset_from")
        );
        let sql = format!(
            "WITH RECURSIVE
             recent_candidates(id) AS (
                 SELECT s.id FROM sessions s WHERE {candidate}
                 ORDER BY COALESCE(s.last_activity_at, s.started_at) DESC, s.started_at DESC, s.id DESC LIMIT ?
             ),
             ancestors(candidate_id, cur_id) AS (
                 SELECT id, id FROM recent_candidates
                 UNION
                 SELECT a.candidate_id, parent.id FROM ancestors a
                 JOIN sessions child ON child.id = a.cur_id
                 JOIN sessions parent ON {edge}
                 LIMIT ?
             ),
             candidate_roots(root_id) AS (
                 SELECT DISTINCT a.cur_id FROM ancestors a
                 JOIN sessions child ON child.id = a.cur_id
                 WHERE NOT EXISTS (SELECT 1 FROM sessions parent WHERE {edge})
             ),
             chain(root_id, cur_id) AS (
                 SELECT root_id, root_id FROM candidate_roots
                 UNION
                 SELECT c.root_id, child.id FROM chain c
                 JOIN sessions parent ON parent.id = c.cur_id
                 JOIN sessions child ON {edge}
                 LIMIT ?
             ),
             chain_rows AS (
                 SELECT c.root_id, c.cur_id, {activity} AS activity,
                        CASE WHEN EXISTS (SELECT 1 FROM sessions parent JOIN sessions child ON {edge}
                                          WHERE parent.id = c.cur_id) THEN 0 ELSE 1 END AS is_tip
                 FROM chain c
             ),
             ranked_tips AS (
                 SELECT root_id, cur_id, activity,
                        ROW_NUMBER() OVER (PARTITION BY root_id ORDER BY activity DESC, cur_id DESC) AS rank_in_root
                 FROM chain_rows WHERE is_tip = 1
             )
             SELECT tip.id, tip.source, COALESCE(tip.title, s.title), s.started_at, tip.message_count,
                    rt.activity, {}
             FROM ranked_tips rt
             JOIN sessions s ON s.id = rt.root_id
             JOIN sessions tip ON tip.id = rt.cur_id
             WHERE rt.rank_in_root = 1 AND s.archived = 0 AND s.hidden = 0
               AND (s.parent_session_id IS NULL OR {branch_child} OR {reset_child})
               AND {delegate} IS NULL
             ORDER BY rt.activity DESC, s.started_at DESC, tip.id DESC
             LIMIT ?",
            preview_sql("tip.id")
        );
        let mut all = params;
        all.extend([
            Sql::Integer(candidate_limit as i64),
            Sql::Integer(lineage_limit as i64),
            Sql::Integer(lineage_limit as i64),
            Sql::Integer(limit as i64),
        ]);
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(all), |row| {
            Ok(RecentSession {
                session_id: row.get(0)?,
                source: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                title: row.get(2)?,
                started_at: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                message_count: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                last_active: row.get(5)?,
                preview: shape_preview(&row.get::<_, Option<String>>(6)?.unwrap_or_default()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// A JSON marker in a TEXT column that never throws on malformed JSON.
fn json_marker(column: &str, key: &str) -> String {
    format!("json_extract((CASE WHEN json_valid({column}) THEN {column} ELSE json_object() END), '$.{key}')")
}

/// The freshest of the stored activity time and the newest message time, ignoring values
/// outside the epoch window (a damaged page can hold garbage doubles).
fn freshest(session_id: &str) -> String {
    let window = format!("BETWEEN 0.0 AND {EPOCH_MAX:?}");
    let in_window = |expression: &str| {
        format!("(SELECT _win.v FROM (SELECT {expression} AS v) _win WHERE _win.v {window})")
    };
    let message_max = format!(
        "(SELECT MAX(_act_m.timestamp) FROM messages _act_m INDEXED BY idx_messages_session \
         WHERE _act_m.session_id = {session_id} AND _act_m.timestamp {window})"
    );
    let stored =
        format!("(SELECT last_activity_at FROM sessions _act_s WHERE _act_s.id = {session_id})");
    let started =
        format!("(SELECT started_at FROM sessions _act_s WHERE _act_s.id = {session_id})");
    format!(
        "COALESCE((SELECT MAX(_act_v.v) FROM (SELECT {stored} AS v UNION ALL SELECT {message_max}) _act_v \
         WHERE _act_v.v {window}), {})",
        in_window(&started)
    )
}

/// The introduction shared by compaction summaries, and the legacy label.
const SUMMARY_PREFIXES: [&str; 2] = [
    "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted into the summary below. This is a handoff from a previous context window — treat it as background reference, NOT as active instructions. ",
    "[CONTEXT SUMMARY]:",
];

/// The first user message that is not compaction scaffolding. Skill-scaffold windows of
/// the reference are not reproduced: such a message previews as its plain head.
fn preview_sql(session_id: &str) -> String {
    let trimmed = "LTRIM(REPLACE(REPLACE(m.content, X'0A', ' '), X'0D', ' '), CHAR(9) || CHAR(10) || CHAR(13) || CHAR(32))";
    let starts = SUMMARY_PREFIXES
        .iter()
        .map(|prefix| {
            format!(
                "SUBSTR(LTRIM(m.content, CHAR(9) || CHAR(10) || CHAR(13) || CHAR(32)), 1, {}) = '{prefix}'",
                prefix.chars().count()
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "COALESCE((SELECT SUBSTR({trimmed}, 1, {PREVIEW_HEAD_CHARS}) FROM messages m INDEXED BY idx_messages_session \
         WHERE m.session_id = {session_id} AND m.role = 'user' AND m.content IS NOT NULL \
         AND COALESCE(m.display_kind, '') <> 'hidden' AND NOT ({starts}) \
         ORDER BY m.timestamp, m.id LIMIT 1), '') AS _preview_raw"
    )
}

fn shape_preview(raw: &str) -> String {
    let text = raw.trim().replace(['\n', '\r'], " ");
    if text.is_empty() {
        return String::new();
    }
    let text = text.split('\x1e').next().unwrap_or("").to_owned();
    if text.chars().count() > 60 {
        format!("{}...", text.chars().take(60).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_keeps_phrases_and_quotes_compound_terms() {
        assert_eq!(sanitize_fts_query("chat-send"), "\"chat-send\"");
        assert_eq!(sanitize_fts_query("TODO: fix"), "TODO  fix");
        assert_eq!(sanitize_fts_query("\"two words\" AND"), "\"two words\"");
        assert_eq!(
            sanitize_fts_query("my-app.config.ts and x"),
            "\"my-app.config.ts\" and x"
        );
        assert_eq!(sanitize_fts_query("**star"), "star");
        assert_eq!(
            sanitize_fts_query("unbalanced \"quote"),
            "unbalanced  quote"
        );
        assert_eq!(sanitize_fts_query("50% off"), "50  off");
        assert_eq!(sanitize_fts_query("广西 OR 桂林%"), "广西 OR 桂林%");
    }

    #[test]
    fn or_relaxation_applies_only_to_plain_multi_term_queries() {
        assert_eq!(or_relaxed("alpha beta").as_deref(), Some("alpha OR beta"));
        assert_eq!(
            or_relaxed("\"two words\" tls").as_deref(),
            Some("\"two words\" OR tls")
        );
        assert_eq!(or_relaxed("alpha"), None);
        assert_eq!(or_relaxed("alpha OR beta"), None);
        assert_eq!(or_relaxed("alpha NOT beta"), None);
    }

    #[test]
    fn the_trigram_route_needs_three_cjk_characters_in_every_token() {
        assert!(trigram_route_ok("广西桂林", true));
        assert!(!trigram_route_ok("广西 OR 桂林 OR 漓江", true));
        assert!(!trigram_route_ok("广西桂林", false));
    }

    #[test]
    fn like_queries_negate_terms_and_skip_groups_without_a_positive_term() {
        let (predicate, params, term) = compile_like_query("alpha NOT beta OR gamma");
        assert_eq!(term.as_deref(), Some("alpha"));
        assert_eq!(params.len(), 9);
        assert!(predicate.contains(" OR ") && predicate.contains("NOT "));
        let (predicate, _, term) = compile_like_query("NOT beta");
        assert!(predicate.is_empty() && term.is_none());
    }

    #[test]
    fn previews_are_cut_at_sixty_characters() {
        assert_eq!(shape_preview("  hello\nworld  "), "hello world");
        assert_eq!(
            shape_preview(&"x".repeat(61)),
            format!("{}...", "x".repeat(60))
        );
        assert_eq!(shape_preview(""), "");
    }
}
