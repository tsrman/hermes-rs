use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
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
    replay_row_groups: Vec<Vec<i64>>,
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
        let RepairedHistory {
            raw_ids: replay_ids,
            row_groups: replay_row_groups,
            messages,
        } = read_history(&transaction, &id)?;
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
            replay_row_groups,
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
        let RepairedHistory {
            raw_ids: ids,
            row_groups,
            messages,
        } = read_history(&transaction, &self.id)?;
        if ids != self.replay_ids
            || row_groups != self.replay_row_groups
            || messages != turn.replayed_history
        {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        require_idle(&transaction, &self.root, &self.id, now)?;
        let (active_count, prior_tools) = if let Some(compaction) = &turn.compaction {
            let mut rows: Vec<i64> = row_groups
                .get(compaction.range.clone())
                .ok_or(StorageError::InvalidTurn)?
                .iter()
                .flatten()
                .copied()
                .collect();
            rows.sort_unstable();
            rows.dedup();
            let (active_count, tools) = compression::archive_middle(
                &transaction,
                &self.id,
                &rows,
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
        let RepairedHistory {
            raw_ids: ids,
            row_groups,
            messages,
        } = read_history(&transaction, &self.id)?;
        if ids != self.replay_ids
            || row_groups != self.replay_row_groups
            || messages != history.messages
        {
            return Err(StorageError::SessionChanged(self.id.clone()));
        }
        require_idle(&transaction, &self.root, &self.id, now)?;
        let mut rows: Vec<i64> = row_groups
            .get(compaction.range.clone())
            .ok_or(StorageError::InvalidTurn)?
            .iter()
            .flatten()
            .copied()
            .collect();
        rows.sort_unstable();
        rows.dedup();
        let archived = rows.len();
        let (active_count, tools) = compression::archive_middle(
            &transaction,
            &self.id,
            &rows,
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

static INTERNAL_CONTEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<\s*memory-context\s*>[\s\S]*?</\s*memory-context\s*>").unwrap()
});
static INTERNAL_NOTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\[System note:\s*The following is recalled memory context,\s*NOT new user input\.\s*Treat as (?:informational background data|authoritative reference data[^\]]*)\.\]\s*",
    )
    .unwrap()
});
static FENCE_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?\s*memory-context\s*>").unwrap());

fn sanitize_context(text: &str) -> String {
    let s = INTERNAL_CONTEXT_RE.replace_all(text, "");
    let s = INTERNAL_NOTE_RE.replace_all(&s, "");
    let s = FENCE_TAG_RE.replace_all(&s, "");
    s.trim().to_owned()
}

fn tool_call_id_variants(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = vec![trimmed.to_owned()];
    if let Some((left, right)) = trimmed.split_once('|') {
        let l = left.trim();
        let r = right.trim();
        if !l.is_empty() && !out.iter().any(|v| v == l) {
            out.push(l.to_owned());
        }
        if !r.is_empty() && !out.iter().any(|v| v == r) {
            out.push(r.to_owned());
        }
    }
    out
}

fn call_variants_from_value(call: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["id", "call_id", "response_item_id"] {
        if let Some(id) = call.get(key).and_then(Value::as_str) {
            for v in tool_call_id_variants(id) {
                if !out.iter().any(|existing| existing == &v) {
                    out.push(v);
                }
            }
        }
    }
    out
}

fn msg_has_non_tool_payload(msg: &Value) -> bool {
    match msg.get("content") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        _ => false,
    }
}

struct LoadedRow {
    row_ids: Vec<i64>,
    message: Value,
    display_kind: Option<String>,
    compressed_summary: bool,
    finish_reason: Option<String>,
}

fn absorb_ids(target: &mut Vec<i64>, extra: &[i64]) {
    for &id in extra {
        if !target.contains(&id) {
            target.push(id);
        }
    }
}

struct RepairedHistory {
    raw_ids: Vec<i64>,
    row_groups: Vec<Vec<i64>>,
    messages: Vec<Value>,
}

/// Read the active transcript of `id` and apply upstream `session_rules/turns.py`
/// `repair_alternation` (4 passes: merge consecutive assistants, drop stray tool results,
/// prune unanswered tool calls & drop empty assistant turns, merge consecutive plain-text users).
fn read_history(connection: &Connection, id: &str) -> Result<RepairedHistory, StorageError> {
    let mut statement = connection.prepare(
        "SELECT role, content, api_content, tool_calls, tool_call_id, id,
                display_kind, _compressed_summary, finish_reason
         FROM messages
         WHERE session_id = ?1 AND active = 1
           AND role IN ('user', 'assistant', 'tool', 'system', 'developer')
           AND COALESCE(display_kind, '') NOT IN ('content_refused', 'hidden', 'failed_turn')
           AND COALESCE(json_extract(CASE WHEN json_valid(display_metadata) THEN display_metadata ELSE '{}' END,
                                     '$._queued_prompt'), 0) != 1
         ORDER BY id",
    )?;
    let invalid = || StorageError::InvalidReplay(id.into());
    let mut rows = statement.query([id])?;
    let mut raw_ids = Vec::new();
    let mut loaded: Vec<LoadedRow> = Vec::new();
    while let Some(row) = rows.next()? {
        let role: String = row.get(0)?;
        let api_content: Option<String> = row.get(2)?;
        let content = match api_content {
            Some(exact) => Value::String(exact),
            None => match read_content(row, 1)? {
                Value::String(text) if matches!(role.as_str(), "user" | "assistant") => {
                    Value::String(sanitize_context(&text))
                }
                other => other,
            },
        };
        let row_id: i64 = row.get(5)?;
        let display_kind: Option<String> = row.get(6)?;
        let compressed_summary: bool = row.get::<_, Option<bool>>(7)?.unwrap_or(false);
        let finish_reason: Option<String> = row.get(8)?;
        raw_ids.push(row_id);

        let mut message = json!({"role": role, "content": content});
        if role == "assistant" {
            if let Some(encoded) = row.get::<_, Option<String>>(3)? {
                let calls: Vec<Value> = serde_json::from_str(&encoded).map_err(|_| invalid())?;
                if !calls.is_empty() {
                    let normalized = calls
                        .iter()
                        .map(|call| {
                            let call_id = call["id"]
                                .as_str()
                                .filter(|id| !id.is_empty())
                                .ok_or_else(invalid)?;
                            let name = call["function"]["name"].as_str().ok_or_else(invalid)?;
                            let args = call["function"]["arguments"]
                                .as_str()
                                .ok_or_else(invalid)?;
                            Ok(json!({"id": call_id, "type": "function", "function": {"name": name, "arguments": args}}))
                        })
                        .collect::<Result<Vec<_>, StorageError>>()?;
                    message["tool_calls"] = Value::Array(normalized);
                }
            }
        } else if role == "tool" {
            if let Some(call_id) = row.get::<_, Option<String>>(4)? {
                if !call_id.trim().is_empty() {
                    message["tool_call_id"] = Value::String(call_id);
                }
            }
        }
        loaded.push(LoadedRow {
            row_ids: vec![row_id],
            message,
            display_kind,
            compressed_summary,
            finish_reason,
        });
    }

    // Pass 0: merge consecutive assistant turns (verification candidates superseded, others folded).
    let mut pass0: Vec<LoadedRow> = Vec::with_capacity(loaded.len());
    for mut item in loaded {
        let is_assistant = item.message["role"] == "assistant";
        if is_assistant
            && pass0
                .last()
                .is_some_and(|prev| prev.message["role"] == "assistant")
        {
            let prev = pass0.last_mut().unwrap();
            if matches!(
                prev.finish_reason.as_deref(),
                Some("verification_required" | "verify_hook_continue")
            ) {
                absorb_ids(&mut item.row_ids, &prev.row_ids);
                *prev = item;
            } else {
                // Merge tool_calls
                let mut prev_calls = prev
                    .message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let new_calls = item
                    .message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !new_calls.is_empty() {
                    prev_calls.extend(new_calls);
                    prev.message["tool_calls"] = Value::Array(prev_calls);
                } else if prev_calls.is_empty() {
                    if let Some(obj) = prev.message.as_object_mut() {
                        obj.remove("tool_calls");
                    }
                }
                // Merge plain-text content
                match (
                    prev.message.get("content").cloned(),
                    item.message.get("content").cloned(),
                ) {
                    (Some(Value::String(a)), Some(Value::String(b))) => {
                        let a_t = a.trim();
                        let b_t = b.trim();
                        let joined = match (a_t.is_empty(), b_t.is_empty()) {
                            (true, true) => String::new(),
                            (true, false) => b_t.to_owned(),
                            (false, true) => a_t.to_owned(),
                            (false, false) => format!("{a_t}\n{b_t}"),
                        };
                        prev.message["content"] = Value::String(joined);
                    }
                    (None | Some(Value::Null), Some(b)) => {
                        prev.message["content"] = b;
                    }
                    _ => {}
                }
                absorb_ids(&mut prev.row_ids, &item.row_ids);
            }
            continue;
        }
        pass0.push(item);
    }

    // Pass 1: drop stray tool results not matching an unanswered tool call from the preceding assistant.
    let mut pass1: Vec<LoadedRow> = Vec::with_capacity(pass0.len());
    let mut leading_drops: Vec<i64> = Vec::new();
    let mut known_tool_ids: HashMap<String, usize> = HashMap::new();
    let mut matched_groups: HashSet<usize> = HashSet::new();
    let mut next_group: usize = 0;
    for item in pass0 {
        let role = item.message["role"].as_str().unwrap_or("");
        if role == "assistant" || role == "user" {
            known_tool_ids.clear();
            matched_groups.clear();
            if role == "assistant" {
                if let Some(calls) = item.message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let variants = call_variants_from_value(call);
                        if !variants.is_empty() {
                            for v in variants {
                                known_tool_ids.entry(v).or_insert(next_group);
                            }
                            next_group += 1;
                        }
                    }
                }
            }
            pass1.push(item);
        } else if role == "tool" {
            let result_variants = item
                .message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(tool_call_id_variants)
                .unwrap_or_default();
            let candidate_group = result_variants
                .iter()
                .filter_map(|v| known_tool_ids.get(v).copied())
                .filter(|g| !matched_groups.contains(g))
                .min();
            match candidate_group {
                Some(group) => {
                    matched_groups.insert(group);
                    pass1.push(item);
                }
                None => {
                    if let Some(last) = pass1.last_mut() {
                        absorb_ids(&mut last.row_ids, &item.row_ids);
                    } else {
                        absorb_ids(&mut leading_drops, &item.row_ids);
                    }
                }
            }
        } else {
            pass1.push(item);
        }
    }
    if let Some(first) = pass1.first_mut() {
        absorb_ids(&mut first.row_ids, &leading_drops);
    }

    // Pass 2: prune unanswered tool_calls on assistant turns; drop empty assistant turns.
    let mut pass2: Vec<LoadedRow> = Vec::with_capacity(pass1.len());
    let mut leading_drops2: Vec<i64> = Vec::new();
    for i in 0..pass1.len() {
        let has_calls = pass1[i].message["role"] == "assistant"
            && pass1[i]
                .message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
        if !has_calls {
            pass2.push(LoadedRow {
                row_ids: pass1[i].row_ids.clone(),
                message: pass1[i].message.clone(),
                display_kind: pass1[i].display_kind.clone(),
                compressed_summary: pass1[i].compressed_summary,
                finish_reason: pass1[i].finish_reason.clone(),
            });
            continue;
        }
        let mut answered: HashSet<String> = HashSet::new();
        for follower in &pass1[i + 1..] {
            if follower.message["role"] != "tool" {
                break;
            }
            if let Some(tid) = follower.message.get("tool_call_id").and_then(Value::as_str) {
                for v in tool_call_id_variants(tid) {
                    answered.insert(v);
                }
            }
        }
        let mut item = LoadedRow {
            row_ids: pass1[i].row_ids.clone(),
            message: pass1[i].message.clone(),
            display_kind: pass1[i].display_kind.clone(),
            compressed_summary: pass1[i].compressed_summary,
            finish_reason: pass1[i].finish_reason.clone(),
        };
        let kept_calls: Vec<Value> = item
            .message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|call| {
                call_variants_from_value(call)
                    .iter()
                    .any(|v| answered.contains(v))
            })
            .collect();
        if kept_calls.is_empty() && !msg_has_non_tool_payload(&item.message) {
            if let Some(last) = pass2.last_mut() {
                absorb_ids(&mut last.row_ids, &item.row_ids);
            } else {
                absorb_ids(&mut leading_drops2, &item.row_ids);
            }
            continue;
        }
        if kept_calls.is_empty() {
            if let Some(obj) = item.message.as_object_mut() {
                obj.remove("tool_calls");
            }
        } else {
            item.message["tool_calls"] = Value::Array(kept_calls);
        }
        pass2.push(item);
    }
    if let Some(first) = pass2.first_mut() {
        absorb_ids(&mut first.row_ids, &leading_drops2);
    }

    // Pass 3: merge consecutive plain-text user turns (preserving summary carriers and steer rows).
    let mut pass3: Vec<LoadedRow> = Vec::with_capacity(pass2.len());
    for item in pass2 {
        let can_merge = item.message["role"] == "user"
            && !item.compressed_summary
            && item
                .message
                .get("content")
                .and_then(Value::as_str)
                .is_some()
            && pass3.last().is_some_and(|prev| {
                prev.message["role"] == "user"
                    && !prev.compressed_summary
                    && prev.display_kind.as_deref() != Some("steer")
                    && prev
                        .message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some()
            });
        if can_merge {
            let prev = pass3.last_mut().unwrap();
            let a = prev.message["content"].as_str().unwrap_or("");
            let b = item.message["content"].as_str().unwrap_or("");
            let joined = match (a.is_empty(), b.is_empty()) {
                (true, true) => String::new(),
                (true, false) => b.to_owned(),
                (false, true) => a.to_owned(),
                (false, false) => format!("{a}\n\n{b}"),
            };
            prev.message["content"] = Value::String(joined);
            absorb_ids(&mut prev.row_ids, &item.row_ids);
            continue;
        }
        pass3.push(item);
    }

    let mut row_groups = Vec::with_capacity(pass3.len());
    let mut messages = Vec::with_capacity(pass3.len());
    for item in pass3 {
        row_groups.push(item.row_ids);
        messages.push(item.message);
    }
    Ok(RepairedHistory {
        raw_ids,
        row_groups,
        messages,
    })
}
