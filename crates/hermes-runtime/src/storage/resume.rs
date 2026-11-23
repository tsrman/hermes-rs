use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::write::{insert_messages, open_write, timestamp, verify_write_schema, RANDOM_UID};
use super::{compression, read_content, StorageError};
use crate::turn::{CompletedTurn, TurnHistory};

const LEASE_SECONDS: f64 = 300.0;

pub(super) struct ResumeState {
    connection: Connection,
    requested: String,
    id: String,
    root: String,
    holder: String,
    revision: Revision,
    history: TurnHistory,
    replay_ids: Vec<i64>,
    released: bool,
}

#[derive(PartialEq)]
struct Revision {
    started_at: f64,
    ended_at: Option<f64>,
    end_reason: Option<String>,
    runtime_revision: i64,
    runtime_generation: i64,
    transcript_revision: i64,
    active_count: i64,
    last_id: i64,
    model: Option<String>,
    prompt: Option<String>,
}

impl ResumeState {
    pub(super) fn acquire(path: &Path, requested: &str) -> Result<Self, StorageError> {
        let mut connection = open_write(path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_write_schema(&transaction)?;
        let id = compression::tip(&transaction, requested)?;
        let revision = read_revision(&transaction, &id)?;
        if revision.end_reason.as_deref() == Some("compression") {
            return Err(StorageError::NoCompressionSuccessor(id));
        }
        let root = compression::root(&transaction, &id)?;
        let now = timestamp();
        require_idle(&transaction, &root, &id, now)?;
        let holder: String = transaction.query_row(RANDOM_UID, [], |row| row.get(0))?;
        let claimed = transaction.execute(
            "INSERT INTO session_turn_leases(conversation_id, holder, acquired_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(conversation_id) DO UPDATE SET
                holder = excluded.holder, acquired_at = excluded.acquired_at, expires_at = excluded.expires_at
             WHERE session_turn_leases.expires_at <= excluded.acquired_at",
            params![root, holder, now, now + LEASE_SECONDS],
        )?;
        if claimed != 1 {
            return Err(StorageError::SessionBusy(id));
        }
        let (replay_ids, messages) = read_history(&transaction, &id)?;
        let history = TurnHistory {
            system_prompt: revision.prompt.clone().filter(|prompt| !prompt.is_empty()),
            messages,
            ..TurnHistory::default()
        };
        transaction.commit()?;
        Ok(Self {
            connection,
            requested: requested.into(),
            id,
            root,
            holder,
            revision,
            history,
            replay_ids,
            released: false,
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn model(&self) -> Option<&str> {
        self.revision.model.as_deref()
    }

    pub(super) fn take_history(&mut self) -> TurnHistory {
        std::mem::take(&mut self.history)
    }

    pub(super) fn refresh(&self) -> Result<(), StorageError> {
        let now = timestamp();
        let changed = self.connection.execute(
            "UPDATE session_turn_leases SET expires_at = ?3
             WHERE conversation_id = ?1 AND holder = ?2 AND expires_at > ?4",
            params![self.root, self.holder, now + LEASE_SECONDS, now],
        )?;
        if changed != 1 {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        Ok(())
    }

    pub(super) fn append(
        &mut self,
        turn: &CompletedTurn,
        started_at: f64,
        ended_at: f64,
    ) -> Result<String, StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_write_schema(&transaction)?;
        let now = timestamp();
        let owns_lease: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM session_turn_leases
                            WHERE conversation_id = ?1 AND holder = ?2 AND expires_at > ?3)",
            params![self.root, self.holder, now],
            |row| row.get(0),
        )?;
        if !owns_lease
            || compression::tip(&transaction, &self.requested)? != self.id
            || compression::root(&transaction, &self.id)? != self.root
            || read_revision(&transaction, &self.id)? != self.revision
        {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        let (ids, messages) = read_history(&transaction, &self.id)?;
        if ids != self.replay_ids || messages != turn.replayed_history {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        require_idle(&transaction, &self.root, &self.id, now)?;
        let (active_count, prior_tools) = if let Some(compaction) = &turn.compaction {
            let rows = ids
                .get(compaction.range.clone())
                .ok_or(StorageError::InvalidTurn)?;
            let (active_count, tools) = compression::archive_middle(
                &transaction,
                &self.id,
                rows,
                &compaction.summary,
                &compaction.placement,
                &turn.system_prompt,
            )?;
            (active_count, Some(tools))
        } else {
            (self.revision.active_count, None)
        };
        let tool_count =
            insert_messages(&transaction, &self.id, &turn.messages, started_at, ended_at)?;
        transaction.execute(
            "UPDATE sessions SET model = ?2,
                 system_prompt = CASE WHEN system_prompt_hash IS NULL THEN COALESCE(system_prompt, ?3) ELSE system_prompt END,
                 last_activity_at = MAX(COALESCE(last_activity_at, 0), ?4),
                 message_count = ?5 + ?6, tool_call_count = COALESCE(?8, tool_call_count, 0) + ?7
             WHERE id = ?1",
            params![self.id, turn.model, turn.system_prompt, ended_at,
                    active_count, turn.messages.len(), tool_count, prior_tools],
        )?;
        release(&transaction, &self.root, &self.holder)?;
        transaction.commit()?;
        self.released = true;
        Ok(self.id.clone())
    }

    /// Commit a standalone manual compaction (e.g. REPL `/compress`) without appending a new turn.
    /// Returns the number of archived messages.
    pub(super) fn commit_compaction(
        &mut self,
        history: &TurnHistory,
        ended_at: f64,
    ) -> Result<usize, StorageError> {
        let compaction = history
            .compaction
            .as_ref()
            .ok_or(StorageError::InvalidTurn)?;
        let system_prompt = history
            .system_prompt
            .as_deref()
            .ok_or(StorageError::InvalidTurn)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_write_schema(&transaction)?;
        let now = timestamp();
        let owns_lease: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM session_turn_leases
                            WHERE conversation_id = ?1 AND holder = ?2 AND expires_at > ?3)",
            params![self.root, self.holder, now],
            |row| row.get(0),
        )?;
        if !owns_lease
            || compression::tip(&transaction, &self.requested)? != self.id
            || compression::root(&transaction, &self.id)? != self.root
            || read_revision(&transaction, &self.id)? != self.revision
        {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        let (ids, messages) = read_history(&transaction, &self.id)?;
        if ids != self.replay_ids || messages != history.messages {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        require_idle(&transaction, &self.root, &self.id, now)?;
        let rows = ids
            .get(compaction.range.clone())
            .ok_or(StorageError::InvalidTurn)?;
        let archived = rows.len();
        let (active_count, tools) = compression::archive_middle(
            &transaction,
            &self.id,
            rows,
            &compaction.summary,
            &compaction.placement,
            system_prompt,
        )?;
        transaction.execute(
            "UPDATE sessions SET last_activity_at = MAX(COALESCE(last_activity_at, 0), ?2),
                 message_count = ?3, tool_call_count = ?4
             WHERE id = ?1",
            params![self.id, ended_at, active_count, tools],
        )?;
        release(&transaction, &self.root, &self.holder)?;
        transaction.commit()?;
        self.released = true;
        Ok(archived)
    }
}

impl Drop for ResumeState {
    fn drop(&mut self) {
        if !self.released {
            // Drop also runs when the provider future is cancelled or the transcript changes.
            let _ = release(&self.connection, &self.root, &self.holder);
        }
    }
}

fn release(connection: &Connection, root: &str, holder: &str) -> rusqlite::Result<()> {
    connection.execute(
        "DELETE FROM session_turn_leases WHERE conversation_id = ?1 AND holder = ?2",
        params![root, holder],
    )?;
    Ok(())
}

fn require_idle(
    connection: &Connection,
    root: &str,
    id: &str,
    now: f64,
) -> Result<(), StorageError> {
    let busy: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM compression_locks WHERE session_id IN (?1, ?2) AND expires_at > ?3)
            OR EXISTS(SELECT 1 FROM session_admissions WHERE target_session_id IN (?1, ?2) AND status != 'terminal')
            OR EXISTS(SELECT 1 FROM worker_executions WHERE session_id IN (?1, ?2) AND status != 'terminal')",
        params![root, id, now],
        |row| row.get(0),
    )?;
    if busy {
        return Err(StorageError::SessionBusy(id.into()));
    }
    Ok(())
}

fn read_revision(connection: &Connection, id: &str) -> Result<Revision, StorageError> {
    connection.query_row(
        "SELECT started_at, ended_at, end_reason, runtime_revision, runtime_generation,
                transcript_revision,
                (SELECT count(*) FROM messages WHERE session_id = s.id AND active = 1),
                COALESCE((SELECT max(id) FROM messages WHERE session_id = s.id AND active = 1), 0),
                model, COALESCE((SELECT prompt FROM system_prompts WHERE hash = s.system_prompt_hash), system_prompt)
         FROM sessions s WHERE id = ?1",
        [id],
        |row| Ok(Revision { started_at: row.get(0)?, ended_at: row.get(1)?, end_reason: row.get(2)?,
            runtime_revision: row.get(3)?, runtime_generation: row.get(4)?, transcript_revision: row.get(5)?,
            active_count: row.get(6)?, last_id: row.get(7)?, model: row.get(8)?, prompt: row.get(9)? }),
    ).optional()?.ok_or_else(|| StorageError::SessionNotFound(id.into()))
}

fn read_history(connection: &Connection, id: &str) -> Result<(Vec<i64>, Vec<Value>), StorageError> {
    let mut statement = connection.prepare(
        "SELECT role, content, api_content, tool_calls, tool_call_id, id FROM messages
         WHERE session_id = ?1 AND active = 1 AND role IN ('user', 'assistant', 'tool', 'system', 'developer')
           AND COALESCE(display_kind, '') != 'content_refused'
           AND COALESCE(json_extract(CASE WHEN json_valid(display_metadata) THEN display_metadata ELSE '{}' END,
                                     '$._queued_prompt'), 0) != 1
         ORDER BY id",
    )?;
    let invalid = || StorageError::InvalidReplay(id.into());
    let mut rows = statement.query([id])?;
    let mut ids = Vec::new();
    let mut history = Vec::new();
    let mut pending_calls = HashSet::new();
    while let Some(row) = rows.next()? {
        let role: String = row.get(0)?;
        let content = match row.get::<_, Option<String>>(2)? {
            Some(exact) => Value::String(exact),
            None => read_content(row, 1)?,
        };
        if role != "tool" && !pending_calls.is_empty() {
            return Err(invalid());
        }
        let mut message = json!({"role": role, "content": content});
        if role == "assistant" {
            if let Some(encoded) = row.get::<_, Option<String>>(3)? {
                let calls: Vec<Value> = serde_json::from_str(&encoded).map_err(|_| invalid())?;
                if !calls.is_empty() {
                    let calls = calls.iter().map(|call| {
                        let call_id = call["id"].as_str().filter(|id| !id.is_empty()).ok_or_else(invalid)?;
                        let name = call["function"]["name"].as_str().ok_or_else(invalid)?;
                        let args = call["function"]["arguments"].as_str().ok_or_else(invalid)?;
                        if !pending_calls.insert(call_id.to_owned()) {
                            return Err(invalid());
                        }
                        Ok(json!({"id": call_id, "type": "function", "function": {"name": name, "arguments": args}}))
                    }).collect::<Result<Vec<_>, StorageError>>()?;
                    message["tool_calls"] = Value::Array(calls);
                }
            }
        } else if role == "tool" {
            let call_id: String = row.get::<_, Option<String>>(4)?.ok_or_else(invalid)?;
            if !pending_calls.remove(&call_id) {
                return Err(invalid());
            }
            message["tool_call_id"] = Value::String(call_id);
        }
        history.push(message);
        ids.push(row.get(5)?);
    }
    if !pending_calls.is_empty() {
        return Err(invalid());
    }
    Ok((ids, history))
}
