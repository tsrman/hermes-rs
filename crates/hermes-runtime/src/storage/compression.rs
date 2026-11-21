use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::StorageError;

const CHAIN_LIMIT: usize = 1000;

/// Replace a contiguous span of replayed history within the caller's turn transaction.
pub(super) fn archive_middle(
    transaction: &Transaction<'_>,
    session: &str,
    rows: &[i64],
    summary: &str,
    system_prompt: &str,
) -> Result<(i64, i64), StorageError> {
    let first = rows.first().ok_or(StorageError::InvalidTurn)?;
    let last = rows.last().ok_or(StorageError::InvalidTurn)?;
    transaction.execute(
        "UPDATE messages SET active = 0, compacted = 1
         WHERE session_id = ?1 AND active = 1 AND id BETWEEN ?2 AND ?3",
        params![session, first, last],
    )?;
    let mut tail: Vec<i64> = {
        let mut statement = transaction.prepare(
            "UPDATE messages SET active = 0, compacted = 0
             WHERE session_id = ?1 AND active = 1 AND id > ?2 RETURNING id",
        )?;
        let rows = statement.query_map(params![session, last], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    transaction.execute(
        "INSERT INTO messages(session_id, role, content, timestamp, _compressed_summary)
         SELECT ?1, 'user', ?2, timestamp, 1 FROM messages WHERE id = ?3",
        params![session, summary, first],
    )?;
    if !tail.is_empty() {
        // The summary gets an earlier row ID than the carried tail. Clone the
        // tail's original columns to keep its display metadata and tool identities.
        tail.sort_unstable();
        let columns: String = transaction.query_row(
            r#"SELECT group_concat(printf('"%w"', name), ',') FROM (
                   SELECT name FROM pragma_table_xinfo('messages')
                   WHERE hidden = 0 AND name NOT IN ('id', 'active', 'compacted', 'display_order') ORDER BY cid
               )"#,
            [],
            |row| row.get(0),
        )?;
        let mut clone = transaction.prepare(&format!(
            "INSERT INTO messages ({columns}, active, compacted)
             SELECT {columns}, 1, 0 FROM messages WHERE id = ?1"
        ))?;
        for row in tail {
            clone.execute([row])?;
        }
    }
    transaction.execute(
        "UPDATE sessions SET system_prompt = ?2, system_prompt_hash = NULL,
             prompt_cache_clock = NULL, transcript_revision = transcript_revision + 1
         WHERE id = ?1",
        params![session, system_prompt],
    )?;
    Ok(transaction.query_row(
        "SELECT count(*), COALESCE(sum(CASE
             WHEN role = 'assistant' AND json_valid(tool_calls) THEN json_array_length(tool_calls)
             ELSE 0 END), 0)
         FROM messages WHERE session_id = ?1 AND active = 1",
        [session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

// The pinned Python store prefers an already-compressed continuation, then an open child.
// An explicit reset, branch, or delegation is a separate conversation even with this parent.
// Schema 32 can have unmarked delegated children. Apply the schema 33 lineage correction
// to the read, while keeping a subagent's own compression continuations eligible.
const NEXT_SEGMENT: &str = "
    WITH candidates AS (
        SELECT child.*,
               CASE WHEN json_valid(child.model_config) THEN child.model_config ELSE '{}' END AS config
        FROM sessions AS parent JOIN sessions AS child ON child.parent_session_id = parent.id
        WHERE parent.id = ?1 AND parent.end_reason = 'compression' AND child.source != 'tool'
          AND NOT (child.source = 'subagent' AND parent.source != 'subagent'
                   AND (SELECT version FROM schema_version) = 32)
    )
    SELECT child.id FROM candidates AS child
    WHERE json_extract(child.config, '$._branched_from') IS NULL
      AND json_extract(child.config, '$._delegate_from') IS NULL
      AND json_extract(child.config, '$._reset_from') IS NULL
    ORDER BY CASE WHEN child.end_reason = 'compression' THEN 0
                  WHEN child.ended_at IS NULL THEN 1 ELSE 2 END,
             COALESCE((SELECT MAX(activity) FROM (
                 SELECT child.last_activity_at AS activity
                 UNION ALL
                 SELECT MAX(timestamp) FROM messages
                 WHERE session_id = child.id AND timestamp BETWEEN 0 AND 4200000000
             ) WHERE activity BETWEEN 0 AND 4200000000),
             CASE WHEN child.started_at BETWEEN 0 AND 4200000000 THEN child.started_at END) DESC,
             child.started_at DESC, child.id DESC
    LIMIT 1";

pub(super) fn tip(connection: &Connection, id: &str) -> Result<String, StorageError> {
    walk(connection, id, NEXT_SEGMENT)
}

pub(super) fn root(connection: &Connection, id: &str) -> Result<String, StorageError> {
    walk(
        connection,
        id,
        "
        WITH child AS (
            SELECT parent_session_id, source,
                   CASE WHEN json_valid(model_config) THEN model_config ELSE '{}' END AS config
            FROM sessions WHERE id = ?1
        )
        SELECT parent.id FROM child JOIN sessions parent ON parent.id = child.parent_session_id
        WHERE parent.end_reason = 'compression' AND child.source != 'tool'
          AND COALESCE(json_extract(child.config, '$._branched_from'), '') != parent.id
          AND COALESCE(json_extract(child.config, '$._delegate_from'), '') != parent.id
          AND COALESCE(json_extract(child.config, '$._reset_from'), '') != parent.id",
    )
}

fn walk(connection: &Connection, id: &str, next: &str) -> Result<String, StorageError> {
    let mut statement = connection.prepare(next)?;
    let mut current = id.to_owned();
    let mut seen = HashSet::new();
    while seen.len() <= CHAIN_LIMIT && seen.insert(current.clone()) {
        match statement
            .query_row([&current], |row| row.get(0))
            .optional()?
        {
            Some(child) => current = child,
            None => return Ok(current),
        }
    }
    Err(StorageError::InvalidCompressionChain)
}
