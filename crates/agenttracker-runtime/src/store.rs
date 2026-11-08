//! Persistence helpers for the rt_* tables (migration v004).
//!
//! Every accepted record is also appended to the existing local hash chain (`chain_entries`) in the
//! same transaction, with event id = record id. Threat model: the chain gives tamper *evidence* for
//! casual or accidental edits; a process with write access to the database can rebuild it. The
//! append-only triggers are an application-level guard, not a security boundary.
use crate::Result;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params_from_iter, Connection};
use serde_json::{Map, Value};
use tracker_common::hash_chain::ChainStore;

/// Columns stored as JSON text that are parsed back when read.
const JSON_COLS: &[&str] = &[
    "protected_paths", "provider_config", "limits", "tool_definitions", "compaction", "omitted", "settings", "payload_redacted", "response_redacted",
    "errors", "proposal", "instruction_ids", "evidence_ids", "args", "output_redacted", "detail", "excerpts", "modification", "counts", "limitations", "normalized_action",
    "request_redacted", "tool_calls", "usage",
];

fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n.as_i64().map(SqlValue::Integer).unwrap_or_else(|| SqlValue::Real(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// Inserts `row` into `table` (internal constant names only) and appends it to the hash chain.
pub fn insert(conn: &Connection, table: &'static str, row: Value) -> Result<()> {
    insert_plain(conn, table, &row)?;
    let obj = row.as_object().cloned().unwrap_or_default();
    let id = obj.get("id").and_then(|v| v.as_str()).map(String::from).or_else(|| obj.get("action_id").and_then(|v| v.as_str()).map(|a| format!("{a}#state"))).unwrap_or_default();
    let mut ev = Map::new();
    ev.insert("rt_table".into(), Value::String(table.into()));
    ev.insert("record".into(), Value::Object(obj));
    ChainStore::append_with(conn, &id, &Value::Object(ev), crate::now_ms())?;
    Ok(())
}

/// Inserts `row` into `table` (internal constant names only) without touching the hash chain.
pub fn insert_plain(conn: &Connection, table: &'static str, row: &Value) -> Result<()> {
    let empty = Map::new();
    let obj = row.as_object().unwrap_or(&empty);
    let cols: Vec<&String> = obj.keys().collect();
    let sql = format!("INSERT INTO {table} ({}) VALUES ({})", cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(","), (1..=cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(","));
    conn.execute(&sql, params_from_iter(obj.values().map(to_sql)))?;
    Ok(())
}

pub fn rows(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let mut q = stmt.query(params)?;
    while let Some(r) = q.next()? {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(x) => Value::from(x),
                ValueRef::Real(x) => Value::from(x),
                ValueRef::Text(t) => {
                    let s = String::from_utf8_lossy(t).into_owned();
                    if JSON_COLS.contains(&n.as_str()) {
                        serde_json::from_str(&s).unwrap_or(Value::String(s))
                    } else {
                        Value::String(s)
                    }
                }
                ValueRef::Blob(b) => Value::String(hex::encode(b)),
            };
            m.insert(n.clone(), v);
        }
        out.push(Value::Object(m));
    }
    Ok(out)
}

pub fn row(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Option<Value>> {
    Ok(rows(conn, sql, params)?.into_iter().next())
}

pub fn latest_state(conn: &Connection, action_id: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(conn.query_row("SELECT state FROM rt_action_states WHERE action_id=?1 ORDER BY seq DESC LIMIT 1", [action_id], |r| r.get(0)).optional()?)
}
