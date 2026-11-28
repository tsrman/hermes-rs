//! Workspace-scoped SQLite (`rusqlite`) and safe YAML (`serde_yaml`) helpers for the embedded
//! RustPython sandbox.
//!
//! Keeps connection state per interpreter thread with hard limits on open handles, disables
//! dynamic extension loading, and enforces the caller's workspace/temp scope check for any
//! file-backed database.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::Path;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

const MAX_SQLITE_CONNS: usize = 32;
const FIRST_CONN_ID: i32 = 1;

/// SQLite authorizer callback that blocks `ATTACH`/`DETACH` (preventing creating or opening
/// database files outside the workspace/temp scope) and dangerous schema-tampering pragmas.
unsafe extern "C" fn sandbox_sqlite_authorizer(
    _user_data: *mut c_void,
    action_code: c_int,
    arg1: *const c_char,
    _arg2: *const c_char,
    _db_name: *const c_char,
    _trigger_name: *const c_char,
) -> c_int {
    if action_code == rusqlite::ffi::SQLITE_ATTACH || action_code == rusqlite::ffi::SQLITE_DETACH {
        return rusqlite::ffi::SQLITE_DENY;
    }
    if action_code == rusqlite::ffi::SQLITE_PRAGMA && !arg1.is_null() {
        if let Ok(name) = CStr::from_ptr(arg1).to_str() {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "writable_schema" | "trusted_schema" | "load_extension"
            ) {
                return rusqlite::ffi::SQLITE_DENY;
            }
        }
    }
    rusqlite::ffi::SQLITE_OK
}

fn harden_connection(conn: &Connection) -> Result<(), String> {
    // Disallow attaching any auxiliary database file at the engine level, and install the
    // authorizer to deny ATTACH/DETACH and dangerous pragmas.
    unsafe {
        let handle = conn.handle();
        rusqlite::ffi::sqlite3_limit(handle, rusqlite::ffi::SQLITE_LIMIT_ATTACHED, 0);
        let rc = rusqlite::ffi::sqlite3_set_authorizer(
            handle,
            Some(sandbox_sqlite_authorizer),
            std::ptr::null_mut(),
        );
        if rc != rusqlite::ffi::SQLITE_OK {
            return Err(format!(
                "failed installing sqlite3 sandbox authorizer (rc={rc})"
            ));
        }
    }
    Ok(())
}

/// Reject `VACUUM ... INTO ...` statements (which write an arbitrary filesystem file) while still
/// permitting plain `VACUUM` on the already-scoped connection. Strips SQL comments and quoted
/// literals before inspecting tokens.
fn reject_vacuum_into(sql: &str) -> Result<(), String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut tokens: Vec<String> = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        // Line comment: -- ... \n
        if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: /* ... */
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // Single-quoted or double-quoted literal/identifier
        if b == b'\'' || b == b'"' {
            let quote = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            tokens.push("<lit>".into());
            continue;
        }
        if b == b';' {
            tokens.push(";".into());
            i += 1;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            tokens.push(sql[start..i].to_ascii_lowercase());
            continue;
        }
        i += 1;
    }

    let mut saw_vacuum = false;
    for tok in &tokens {
        match tok.as_str() {
            ";" => saw_vacuum = false,
            "vacuum" => saw_vacuum = true,
            "into" if saw_vacuum => {
                return Err("VACUUM INTO is disabled in the SQLite sandbox".into());
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) struct SqliteStore {
    conns: HashMap<i32, Connection>,
    next_id: i32,
}

impl SqliteStore {
    pub(crate) fn new() -> Self {
        Self {
            conns: HashMap::new(),
            next_id: FIRST_CONN_ID,
        }
    }

    pub(crate) fn open_memory(&mut self) -> Result<i32, String> {
        self.check_limit()?;
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        harden_connection(&conn)?;
        Ok(self.insert(conn))
    }

    pub(crate) fn open_file(&mut self, path: &Path, read_only: bool) -> Result<i32, String> {
        self.check_limit()?;
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
        };
        let conn = Connection::open_with_flags(path, flags).map_err(|e| e.to_string())?;
        harden_connection(&conn)?;
        Ok(self.insert(conn))
    }

    fn check_limit(&self) -> Result<(), String> {
        if self.conns.len() >= MAX_SQLITE_CONNS {
            return Err("too many open sqlite3 connections in sandbox".into());
        }
        Ok(())
    }

    fn insert(&mut self, conn: Connection) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(FIRST_CONN_ID);
        self.conns.insert(id, conn);
        id
    }

    pub(crate) fn close(&mut self, id: i32) {
        self.conns.remove(&id);
    }

    pub(crate) fn in_transaction(&self, id: i32) -> Result<bool, String> {
        let conn = self
            .conns
            .get(&id)
            .ok_or_else(|| "cannot operate on a closed database".to_owned())?;
        Ok(!conn.is_autocommit())
    }

    pub(crate) fn execute_batch(&mut self, id: i32, sql: &str) -> Result<(), String> {
        reject_vacuum_into(sql)?;
        let conn = self
            .conns
            .get_mut(&id)
            .ok_or_else(|| "cannot operate on a closed database".to_owned())?;
        conn.execute_batch(sql).map_err(|e| e.to_string())
    }

    pub(crate) fn execute(
        &mut self,
        id: i32,
        sql: &str,
        params_json: &str,
    ) -> Result<Value, String> {
        reject_vacuum_into(sql)?;
        let conn = self
            .conns
            .get_mut(&id)
            .ok_or_else(|| "cannot operate on a closed database".to_owned())?;
        let raw_params: Value =
            serde_json::from_str(params_json).map_err(|e| format!("invalid params JSON: {e}"))?;

        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let col_names: Vec<String> = stmt.column_names().map(str::to_owned).collect();

        let (named, positional) = decode_params(&raw_params)?;

        if col_names.is_empty() {
            let changes = if let Some(named) = named {
                let refs: Vec<(&str, &dyn rusqlite::ToSql)> = named
                    .iter()
                    .map(|(k, v)| (k.as_str(), v as &dyn rusqlite::ToSql))
                    .collect();
                stmt.execute(refs.as_slice()).map_err(|e| e.to_string())?
            } else {
                let refs: Vec<&dyn rusqlite::ToSql> = positional
                    .iter()
                    .map(|v| v as &dyn rusqlite::ToSql)
                    .collect();
                stmt.execute(refs.as_slice()).map_err(|e| e.to_string())?
            };
            let last_id = conn.last_insert_rowid();
            return Ok(json!({
                "columns": [],
                "rows": [],
                "rowcount": changes,
                "lastrowid": last_id,
            }));
        }

        let mut rows_out: Vec<Value> = Vec::new();
        let col_count = col_names.len();
        let mut collect = |mut rows: rusqlite::Rows<'_>| -> Result<(), String> {
            while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                let mut arr = Vec::with_capacity(col_count);
                for idx in 0..col_count {
                    let val = row.get_ref(idx).map_err(|e| e.to_string())?;
                    arr.push(sql_ref_to_json(val));
                }
                rows_out.push(Value::Array(arr));
            }
            Ok(())
        };

        if let Some(named) = named {
            let refs: Vec<(&str, &dyn rusqlite::ToSql)> = named
                .iter()
                .map(|(k, v)| (k.as_str(), v as &dyn rusqlite::ToSql))
                .collect();
            let rows = stmt.query(refs.as_slice()).map_err(|e| e.to_string())?;
            collect(rows)?;
        } else {
            let refs: Vec<&dyn rusqlite::ToSql> = positional
                .iter()
                .map(|v| v as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query(refs.as_slice()).map_err(|e| e.to_string())?;
            collect(rows)?;
        }

        Ok(json!({
            "columns": col_names,
            "rows": rows_out,
            "rowcount": -1,
            "lastrowid": conn.last_insert_rowid(),
        }))
    }
}

type NamedParams = Option<Vec<(String, SqlValue)>>;

fn decode_params(raw: &Value) -> Result<(NamedParams, Vec<SqlValue>), String> {
    match raw {
        Value::Null => Ok((None, Vec::new())),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_sql(item)?);
            }
            Ok((None, out))
        }
        Value::Object(map) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                let key = if k.starts_with(':') || k.starts_with('@') || k.starts_with('$') {
                    k.clone()
                } else {
                    format!(":{k}")
                };
                out.push((key, json_to_sql(v)?));
            }
            Ok((Some(out), Vec::new()))
        }
        _ => Err("sqlite3 parameters must be a sequence or dict".into()),
    }
}

fn json_to_sql(v: &Value) -> Result<SqlValue, String> {
    match v {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(SqlValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(SqlValue::Real(f))
            } else {
                Err("unsupported numeric parameter".into())
            }
        }
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Object(map) if map.len() == 1 && map.contains_key("__hsr_bytes__") => {
            let b64 = map["__hsr_bytes__"]
                .as_str()
                .ok_or_else(|| "invalid bytes parameter".to_owned())?;
            let bytes = decode_b64(b64)?;
            Ok(SqlValue::Blob(bytes))
        }
        _ => Err("unsupported SQLite parameter type".into()),
    }
}

fn sql_ref_to_json(val: ValueRef<'_>) -> Value {
    match val {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!({"__hsr_bytes__": encode_b64(b)}),
    }
}

pub(crate) fn encode_b64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn decode_b64(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err("invalid base64 byte".into()),
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Parse one or more YAML documents safely into JSON array text using `serde_yaml`.
pub(crate) fn yaml_load_all_to_json(text: &str) -> Result<String, String> {
    use serde::Deserialize;
    let mut docs: Vec<Value> = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(text) {
        let yaml_val = serde_yaml::Value::deserialize(doc).map_err(|e| e.to_string())?;
        let json_val = yaml_value_to_json(yaml_val)?;
        docs.push(json_val);
    }
    serde_json::to_string(&docs).map_err(|e| e.to_string())
}

/// Convert `serde_yaml::Value` to `serde_json::Value` handling non-string YAML mapping keys cleanly.
fn yaml_value_to_json(val: serde_yaml::Value) -> Result<Value, String> {
    match val {
        serde_yaml::Value::Null => Ok(Value::Null),
        serde_yaml::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(json!(i))
            } else if let Some(u) = n.as_u64() {
                Ok(json!(u))
            } else if let Some(f) = n.as_f64() {
                Ok(json!(f))
            } else {
                Ok(Value::Null)
            }
        }
        serde_yaml::Value::String(s) => Ok(Value::String(s)),
        serde_yaml::Value::Sequence(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for item in items {
                arr.push(yaml_value_to_json(item)?);
            }
            Ok(Value::Array(arr))
        }
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s,
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Null => "null".to_owned(),
                    other => serde_yaml::to_string(&other)
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                };
                obj.insert(key, yaml_value_to_json(v)?);
            }
            Ok(Value::Object(obj))
        }
        serde_yaml::Value::Tagged(tagged) => yaml_value_to_json(tagged.value),
    }
}

/// Serialize a JSON text payload to YAML via `serde_yaml`.
pub(crate) fn json_to_yaml(json_text: &str) -> Result<String, String> {
    let val: Value = serde_json::from_str(json_text).map_err(|e| e.to_string())?;
    serde_yaml::to_string(&val).map_err(|e| e.to_string())
}
