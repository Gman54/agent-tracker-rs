//! Durable SQLite-backed ingest queue. Raw hook deliveries are enqueued first, then processed
//! into normalized events; nothing is lost if processing is paused or the app crashes mid-way.
use crate::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: String,
    pub connector: String,
    pub payload: Value,
    pub enqueued_at: i64,
}

#[derive(Clone)]
pub struct Queue {
    conn: Arc<Mutex<Connection>>,
}

impl Queue {
    /// Uses an already-migrated connection (see `Db::shared`).
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Payloads are redacted before they touch disk (the queue is durable storage too).
    pub fn enqueue(&self, connector: &str, payload: &Value) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let payload = tracker_common::redaction::redact_deep(payload);
        self.lock().execute(
            "INSERT INTO queue(id, connector, payload, enqueued_at, delivered) VALUES (?1,?2,?3,?4,0)",
            params![id, connector, payload.to_string(), crate::now_ms()],
        )?;
        Ok(id)
    }

    /// Returns all undelivered entries in enqueue order (does not mark them delivered).
    pub fn drain_pending(&self) -> Result<Vec<QueueEntry>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, connector, payload, enqueued_at FROM queue WHERE delivered = 0 ORDER BY enqueued_at, rowid LIMIT 5000")?;
        let v = st
            .query_map([], |r| {
                Ok(QueueEntry {
                    id: r.get(0)?,
                    connector: r.get(1)?,
                    payload: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
                    enqueued_at: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn mark_delivered(&self, id: &str) -> Result<bool> {
        Ok(self.lock().execute("UPDATE queue SET delivered = 1 WHERE id = ?1", [id])? > 0)
    }

    pub fn pending_count(&self) -> Result<i64> {
        Ok(self.lock().query_row("SELECT COUNT(*) FROM queue WHERE delivered = 0", [], |r| r.get(0))?)
    }

    /// Deletes delivered entries older than `older_than_ms` (housekeeping).
    pub fn purge_delivered(&self, older_than_ms: i64) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM queue WHERE delivered = 1 AND enqueued_at < ?1", [crate::now_ms() - older_than_ms])?)
    }

    /// Imports the hook's offline fallback file (JSON lines written while the collector was not
    /// running). The file is renamed first so concurrent hook appends go to a fresh file.
    pub fn import_fallback_file(&self, path: &Path) -> Result<usize> {
        if !path.exists() {
            return Ok(0);
        }
        let importing = path.with_extension(format!("importing-{}", crate::now_ms()));
        std::fs::rename(path, &importing)?;
        let file = std::fs::File::open(&importing)?;
        let mut n = 0;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
            let connector = v.get("connector").and_then(Value::as_str).unwrap_or("unknown").to_string();
            self.enqueue(&connector, &v)?;
            n += 1;
        }
        std::fs::remove_file(&importing)?;
        Ok(n)
    }
}
