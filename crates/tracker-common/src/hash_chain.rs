//! Append-only local hash chain over stored events (tamper *evidence*, not tamper *proof*:
//! a hostile process with write access to the DB can rebuild the whole chain).
//!
//! event_hash = SHA-256(canonical_json(event))
//! chain_hash = SHA-256(canonical_json({seq, event_id, prev_hash, event_hash, ts}))
//! The first entry's prev_hash is 32 zero bytes.
use crate::canonical::{canonical_hash, canonical_json};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub const GENESIS: [u8; 32] = [0u8; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainEntry {
    pub seq: u64,
    pub event_id: String,
    #[serde(with = "hex32")]
    pub prev_hash: [u8; 32],
    #[serde(with = "hex32")]
    pub event_hash: [u8; 32],
    #[serde(with = "hex32")]
    pub chain_hash: [u8; 32],
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub checked: u64,
    pub first_bad_seq: Option<u64>,
    pub detail: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("corrupt chain row: {0}")]
    Corrupt(String),
}

mod hex32 {
    pub fn serialize<S: serde::Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }
}

pub fn compute_chain_hash(seq: u64, event_id: &str, prev: &[u8; 32], event_hash: &[u8; 32], ts: i64) -> [u8; 32] {
    canonical_hash(&json!({
        "seq": seq,
        "event_id": event_id,
        "prev_hash": hex::encode(prev),
        "event_hash": hex::encode(event_hash),
        "ts": ts,
    }))
}

fn from_hex(s: &str) -> Result<[u8; 32], ChainError> {
    let v = hex::decode(s).map_err(|e| ChainError::Corrupt(e.to_string()))?;
    v.try_into().map_err(|_| ChainError::Corrupt("hash length".into()))
}

pub struct ChainStore {
    conn: Arc<Mutex<Connection>>,
}

pub const CHAIN_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS chain_entries (
    seq INTEGER PRIMARY KEY,
    event_id TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    event_hash TEXT NOT NULL,
    chain_hash TEXT NOT NULL,
    ts INTEGER NOT NULL,
    event_canonical TEXT NOT NULL
);";

impl ChainStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Result<Self, ChainError> {
        conn.lock().unwrap_or_else(|p| p.into_inner()).execute_batch(CHAIN_SCHEMA)?;
        Ok(Self { conn })
    }

    /// Appends an entry inside the caller's connection/transaction (used by the event store so the
    /// event row and its chain entry commit atomically).
    pub fn append_with(conn: &Connection, event_id: &str, event_json: &Value, ts: i64) -> Result<ChainEntry, ChainError> {
        let last: Option<(i64, String)> = conn
            .query_row("SELECT seq, chain_hash FROM chain_entries ORDER BY seq DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (seq, prev_hash) = match last {
            Some((s, h)) => (s as u64 + 1, from_hex(&h)?),
            None => (1, GENESIS),
        };
        let canonical = canonical_json(event_json);
        let event_hash = canonical_hash(event_json);
        let chain_hash = compute_chain_hash(seq, event_id, &prev_hash, &event_hash, ts);
        conn.execute(
            "INSERT INTO chain_entries(seq, event_id, prev_hash, event_hash, chain_hash, ts, event_canonical) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![seq as i64, event_id, hex::encode(prev_hash), hex::encode(event_hash), hex::encode(chain_hash), ts, canonical],
        )?;
        Ok(ChainEntry { seq, event_id: event_id.to_string(), prev_hash, event_hash, chain_hash, ts })
    }

    pub fn append(&self, event_id: &str, event_json: &Value) -> Result<ChainEntry, ChainError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.unchecked_transaction()?;
        let e = Self::append_with(&tx, event_id, event_json, crate::now_ms())?;
        tx.commit()?;
        Ok(e)
    }

    pub fn len(&self) -> Result<u64, ChainError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        Ok(conn.query_row("SELECT COUNT(*) FROM chain_entries", [], |r| r.get::<_, i64>(0))? as u64)
    }

    pub fn is_empty(&self) -> Result<bool, ChainError> {
        Ok(self.len()? == 0)
    }

    /// Verifies entries with `from <= seq <= to`: recomputes each event hash from the stored
    /// canonical event, each chain hash, and the prev-hash link (including the link into `from`).
    pub fn verify_range(&self, from: u64, to: u64) -> Result<VerifyResult, ChainError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut expected_prev: Option<[u8; 32]> = if from <= 1 {
            Some(GENESIS)
        } else {
            conn.query_row("SELECT chain_hash FROM chain_entries WHERE seq = ?1", [from as i64 - 1], |r| r.get::<_, String>(0))
                .optional()?
                .map(|h| from_hex(&h))
                .transpose()?
        };
        let mut stmt = conn.prepare(
            "SELECT seq, event_id, prev_hash, event_hash, chain_hash, ts, event_canonical FROM chain_entries WHERE seq >= ?1 AND seq <= ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![from as i64, to as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?, r.get::<_, String>(6)?))
        })?;
        let mut checked = 0u64;
        let mut last_seq: Option<u64> = None;
        let bad = |seq: u64, checked: u64, detail: String| VerifyResult { ok: false, checked, first_bad_seq: Some(seq), detail };
        for row in rows {
            let (seq, event_id, prev, ev, ch, ts, canon) = row?;
            let seq = seq as u64;
            if let Some(ls) = last_seq {
                if seq != ls + 1 {
                    return Ok(bad(ls + 1, checked, format!("gap: entry {} is missing", ls + 1)));
                }
            }
            let (prev, ev, ch) = (from_hex(&prev)?, from_hex(&ev)?, from_hex(&ch)?);
            if let Some(ep) = expected_prev {
                if ep != prev {
                    return Ok(bad(seq, checked, "previous-hash link broken".into()));
                }
            }
            let value: Value = serde_json::from_str(&canon).map_err(|e| ChainError::Corrupt(e.to_string()))?;
            if canonical_hash(&value) != ev {
                return Ok(bad(seq, checked, "event content does not match its recorded hash".into()));
            }
            if compute_chain_hash(seq, &event_id, &prev, &ev, ts) != ch {
                return Ok(bad(seq, checked, "chain hash mismatch".into()));
            }
            expected_prev = Some(ch);
            last_seq = Some(seq);
            checked += 1;
        }
        Ok(VerifyResult { ok: true, checked, first_bad_seq: None, detail: format!("{checked} entries verified") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_verify_tamper() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let store = ChainStore::new(conn.clone()).unwrap();
        let a = store.append("e1", &json!({"a": 1})).unwrap();
        let b = store.append("e2", &json!({"b": 2})).unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(b.prev_hash, a.chain_hash);
        assert!(store.verify_range(1, 2).unwrap().ok);
        conn.lock().unwrap().execute("UPDATE chain_entries SET event_canonical='{\"a\":9}' WHERE seq=1", []).unwrap();
        let r = store.verify_range(1, 2).unwrap();
        assert!(!r.ok);
        assert_eq!(r.first_bad_seq, Some(1));
    }
}
