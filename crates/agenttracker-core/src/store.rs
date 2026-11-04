//! Persistence for events (deduplicated + hash-chained), manifests, verification results,
//! saved check templates and connector health.
use crate::db::Db;
use crate::event::{AgentEvent, CaptureSource, EventType};
use crate::fs_observer::{EntryType, Manifest, ManifestEntry, ManifestType, Stability};
use crate::verifiers::{CheckResult, CheckStatus};
use crate::Result;
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracker_common::hash_chain::ChainStore;

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct EventStore {
    db: Db,
}

const EVENT_COLS: &str = "id, schema_version, connector, session_id, task_id, tool_call_id, event_type, source_ts, collector_ts, source_seq, working_dir, payload_redacted, capture_source, raw_source, parent_id, connector_version, agent_version, collector_generated";

fn row_to_event(r: &Row) -> rusqlite::Result<AgentEvent> {
    let parse_json = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok());
    Ok(AgentEvent {
        event_id: r.get(0)?,
        schema_version: r.get(1)?,
        connector_name: r.get(2)?,
        session_id: r.get(3)?,
        task_id: r.get(4)?,
        tool_call_id: r.get(5)?,
        event_type: EventType::parse(&r.get::<_, String>(6)?).unwrap_or(EventType::CaptureError),
        source_ts: r.get(7)?,
        collector_ts: r.get(8)?,
        source_seq: r.get(9)?,
        working_dir: r.get(10)?,
        payload_redacted: parse_json(r.get(11)?).unwrap_or(Value::Null),
        capture_source: CaptureSource::parse(&r.get::<_, String>(12)?).unwrap_or(CaptureSource::Hook),
        raw_source: parse_json(r.get(13)?),
        parent_id: r.get(14)?,
        connector_version: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
        agent_version: r.get(16)?,
        collector_generated: r.get::<_, i64>(17)? != 0,
    })
}

impl EventStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Stores an event once. Returns Ok(false) if an event with the same id already exists
    /// (deduplication). Payloads are redacted again here so nothing unredacted reaches disk.
    /// The event row and its hash-chain entry are committed atomically.
    pub fn insert(&self, ev: &AgentEvent) -> Result<bool> {
        let mut ev = ev.clone();
        ev.redact();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        let exists: Option<String> = tx.query_row("SELECT id FROM events WHERE id = ?1", [&ev.event_id], |r| r.get(0)).optional()?;
        if exists.is_some() {
            return Ok(false);
        }
        let entry = ChainStore::append_with(&tx, &ev.event_id, &serde_json::to_value(&ev)?, ev.collector_ts)?;
        tx.execute(
            &format!("INSERT INTO events({EVENT_COLS}, chain_seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)"),
            params![
                ev.event_id,
                ev.schema_version,
                ev.connector_name,
                ev.session_id,
                ev.task_id,
                ev.tool_call_id,
                ev.event_type.as_str(),
                ev.source_ts,
                ev.collector_ts,
                ev.source_seq,
                ev.working_dir,
                ev.payload_redacted.to_string(),
                ev.capture_source.as_str(),
                ev.raw_source.as_ref().map(|v| v.to_string()),
                ev.parent_id,
                ev.connector_version,
                ev.agent_version,
                ev.collector_generated as i64,
                entry.seq as i64,
            ],
        )?;
        if let Some(sid) = &ev.session_id {
            let ts = ev.source_ts.unwrap_or(ev.collector_ts);
            tx.execute(
                "INSERT INTO sessions(id, connector, agent_version, working_dir, started_at, state) VALUES (?1,?2,?3,?4,?5,'active')
                 ON CONFLICT(id) DO UPDATE SET agent_version = COALESCE(excluded.agent_version, sessions.agent_version),
                   working_dir = COALESCE(sessions.working_dir, excluded.working_dir), started_at = MIN(sessions.started_at, excluded.started_at)",
                params![sid, ev.connector_name, ev.agent_version, ev.working_dir, ts],
            )?;
            if ev.event_type == EventType::SessionEnded {
                tx.execute("UPDATE sessions SET ended_at = ?2, state = 'ended' WHERE id = ?1", params![sid, ts])?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    fn query(&self, where_sql: &str, arg: &str) -> Result<Vec<AgentEvent>> {
        let conn = self.db.conn();
        let mut s = conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events WHERE {where_sql} ORDER BY source_seq IS NULL, source_seq, COALESCE(source_ts, collector_ts), collector_ts, id"
        ))?;
        let v = s.query_map([arg], row_to_event)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Events of a session in source order (source_seq, then timestamps).
    pub fn by_session(&self, session_id: &str) -> Result<Vec<AgentEvent>> {
        self.query("session_id = ?1", session_id)
    }

    pub fn by_task(&self, task_id: &str) -> Result<Vec<AgentEvent>> {
        self.query("task_id = ?1", task_id)
    }

    pub fn recent(&self, limit: i64) -> Result<Vec<AgentEvent>> {
        let conn = self.db.conn();
        let mut s = conn.prepare(&format!("SELECT {EVENT_COLS} FROM events ORDER BY collector_ts DESC LIMIT ?1"))?;
        let v = s.query_map([limit], row_to_event)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.db.conn().query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }

    pub fn assign_task(&self, session_id: &str, task_id: &str) -> Result<usize> {
        Ok(self.db.conn().execute("UPDATE events SET task_id = ?2 WHERE session_id = ?1 AND task_id IS NULL", params![session_id, task_id])?)
    }

    pub fn chain(&self) -> Result<ChainStore> {
        Ok(ChainStore::new(self.db.shared())?)
    }
}

// ---------------------------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct ManifestStore {
    db: Db,
}

impl ManifestStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn save(&self, m: &Manifest) -> Result<()> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO manifests(id, task_id, folder_path, captured_at, manifest_type) VALUES (?1,?2,?3,?4,?5)",
            params![m.id, m.task_id.clone().unwrap_or_default(), m.root, m.captured_at, m.manifest_type.as_str()],
        )?;
        {
            let mut st = tx.prepare(
                "INSERT INTO manifest_entries(manifest_id, rel_path, entry_type, size_bytes, sha256, modified_ts, observed_ts, read_ok, error_detail, stability) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for e in &m.entries {
                st.execute(params![
                    m.id,
                    e.rel_path,
                    e.entry_type.as_str(),
                    e.size_bytes.map(|s| s as i64),
                    e.sha256,
                    e.modified_ts,
                    e.observed_ts,
                    e.read_ok as i64,
                    e.error_detail,
                    if e.stability == Stability::Unstable { "unstable" } else { "stable" },
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Option<Manifest>> {
        let conn = self.db.conn();
        let head = conn
            .query_row("SELECT id, task_id, folder_path, captured_at, manifest_type FROM manifests WHERE id = ?1", [id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
            })
            .optional()?;
        let Some((id, task_id, root, captured_at, mt)) = head else { return Ok(None) };
        let mut st = conn.prepare(
            "SELECT rel_path, entry_type, size_bytes, sha256, modified_ts, observed_ts, read_ok, error_detail, stability FROM manifest_entries WHERE manifest_id = ?1 ORDER BY rel_path",
        )?;
        let entries = st
            .query_map([&id], |r| {
                Ok(ManifestEntry {
                    rel_path: r.get(0)?,
                    entry_type: EntryType::parse(&r.get::<_, String>(1)?),
                    size_bytes: r.get::<_, Option<i64>>(2)?.map(|s| s as u64),
                    sha256: r.get(3)?,
                    modified_ts: r.get(4)?,
                    observed_ts: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    read_ok: r.get::<_, i64>(6)? != 0,
                    error_detail: r.get(7)?,
                    stability: if r.get::<_, String>(8)? == "unstable" { Stability::Unstable } else { Stability::Stable },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let root_ok = !entries.iter().any(|e| e.rel_path.is_empty() && !e.read_ok);
        Ok(Some(Manifest {
            id,
            task_id: Some(task_id).filter(|s| !s.is_empty()),
            root,
            captured_at,
            manifest_type: if mt == "baseline" { ManifestType::Baseline } else { ManifestType::Verification },
            entries,
            root_ok,
        }))
    }

    /// Latest manifest per folder for a task and type.
    pub fn latest_for_task(&self, task_id: &str, mt: ManifestType) -> Result<Vec<Manifest>> {
        let ids: Vec<String> = {
            let conn = self.db.conn();
            let mut st = conn.prepare(
                "SELECT m.id FROM manifests m WHERE m.task_id = ?1 AND m.manifest_type = ?2 AND m.captured_at =
                 (SELECT MAX(captured_at) FROM manifests x WHERE x.task_id = m.task_id AND x.folder_path = m.folder_path AND x.manifest_type = m.manifest_type)",
            )?;
            let v = st.query_map(params![task_id, mt.as_str()], |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
            v
        };
        let mut out = Vec::new();
        for id in ids {
            if let Some(m) = self.load(&id)? {
                out.push(m);
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Verification runs / results
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationRun {
    pub id: String,
    pub task_id: String,
    pub definition_version: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub overall_verdict: Option<CheckStatus>,
    pub results: Vec<CheckResult>,
}

#[derive(Clone)]
pub struct ResultStore {
    db: Db,
}

impl ResultStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn save_run(&self, run: &VerificationRun) -> Result<()> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO verification_runs(id, task_id, definition_version, started_at, finished_at, overall_verdict) VALUES (?1,?2,?3,?4,?5,?6)",
            params![run.id, run.task_id, run.definition_version, run.started_at, run.finished_at, run.overall_verdict.map(|s| s.as_str())],
        )?;
        for r in &run.results {
            tx.execute(
                "INSERT OR REPLACE INTO check_results(id, run_id, check_id, check_type, check_version, status, summary, evidence, limitations, started_at, finished_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    format!("{}:{}", run.id, r.check_id),
                    run.id,
                    r.check_id,
                    r.check_type,
                    r.check_version,
                    r.status.as_str(),
                    r.summary,
                    serde_json::to_string(&r.evidence_refs)?,
                    serde_json::to_string(&r.limitations)?,
                    r.started_at,
                    r.finished_at
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<VerificationRun>> {
        let tid: Option<String> = self.db.conn().query_row("SELECT task_id FROM verification_runs WHERE id = ?1", [run_id], |r| r.get(0)).optional()?;
        Ok(match tid {
            Some(t) => self.runs_for_task(&t)?.into_iter().find(|r| r.id == run_id),
            None => None,
        })
    }

    pub fn runs_for_task(&self, task_id: &str) -> Result<Vec<VerificationRun>> {
        let conn = self.db.conn();
        let mut st = conn.prepare("SELECT id, task_id, definition_version, started_at, finished_at, overall_verdict FROM verification_runs WHERE task_id = ?1 ORDER BY started_at DESC")?;
        let mut runs = st
            .query_map([task_id], |r| {
                Ok(VerificationRun {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    definition_version: r.get(2)?,
                    started_at: r.get(3)?,
                    finished_at: r.get(4)?,
                    overall_verdict: r.get::<_, Option<String>>(5)?.and_then(|s| CheckStatus::parse(&s)),
                    results: vec![],
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut rs = conn.prepare(
            "SELECT check_id, check_type, check_version, status, summary, evidence, limitations, started_at, finished_at FROM check_results WHERE run_id = ?1 ORDER BY rowid",
        )?;
        for run in runs.iter_mut() {
            run.results = rs
                .query_map([&run.id], |r| {
                    Ok(CheckResult {
                        check_id: r.get(0)?,
                        check_type: r.get(1)?,
                        check_version: r.get(2)?,
                        status: CheckStatus::parse(&r.get::<_, String>(3)?).unwrap_or(CheckStatus::UnableToVerify),
                        summary: r.get(4)?,
                        evidence_refs: r.get::<_, Option<String>>(5)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                        limitations: r.get::<_, Option<String>>(6)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                        started_at: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                        finished_at: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
        }
        Ok(runs)
    }
}

// ---------------------------------------------------------------------------------------------
// Saved check templates
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckTemplate {
    pub id: String,
    pub name: String,
    pub check_type: String,
    pub params: Value,
    pub created_at: i64,
    pub source: String,
}

#[derive(Clone)]
pub struct TemplateStore {
    db: Db,
}

impl TemplateStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn create(&self, name: &str, check_type: &str, params_v: &Value, source: &str) -> Result<CheckTemplate> {
        let t = CheckTemplate {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            check_type: check_type.to_string(),
            params: params_v.clone(),
            created_at: crate::now_ms(),
            source: source.to_string(),
        };
        self.db.conn().execute(
            "INSERT INTO check_templates(id, name, check_type, params, created_at, source) VALUES (?1,?2,?3,?4,?5,?6)",
            params![t.id, t.name, t.check_type, t.params.to_string(), t.created_at, t.source],
        )?;
        Ok(t)
    }

    pub fn list(&self) -> Result<Vec<CheckTemplate>> {
        let conn = self.db.conn();
        let mut st = conn.prepare("SELECT id, name, check_type, params, created_at, source FROM check_templates ORDER BY created_at DESC")?;
        let v = st
            .query_map([], |r| {
                Ok(CheckTemplate {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    check_type: r.get(2)?,
                    params: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                    created_at: r.get(4)?,
                    source: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<bool> {
        Ok(self.db.conn().execute("UPDATE check_templates SET name = ?2 WHERE id = ?1", params![id, name])? > 0)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        Ok(self.db.conn().execute("DELETE FROM check_templates WHERE id = ?1", [id])? > 0)
    }
}

// ---------------------------------------------------------------------------------------------
// Connector health
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorHealth {
    pub connector: String,
    pub status: String,
    pub agent_version: Option<String>,
    pub capabilities: Value,
    pub last_seen: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct HealthStore {
    db: Db,
}

impl HealthStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn upsert(&self, h: &ConnectorHealth) -> Result<()> {
        self.db.conn().execute(
            "INSERT INTO connector_health(connector, status, agent_version, capabilities, last_seen, last_error) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(connector) DO UPDATE SET status=excluded.status, agent_version=COALESCE(excluded.agent_version, connector_health.agent_version),
               capabilities=excluded.capabilities, last_seen=COALESCE(excluded.last_seen, connector_health.last_seen), last_error=excluded.last_error",
            params![h.connector, h.status, h.agent_version, h.capabilities.to_string(), h.last_seen, h.last_error],
        )?;
        Ok(())
    }

    pub fn touch(&self, connector: &str) -> Result<()> {
        self.db.conn().execute(
            "INSERT INTO connector_health(connector, status, last_seen) VALUES (?1, 'receiving', ?2)
             ON CONFLICT(connector) DO UPDATE SET last_seen = excluded.last_seen",
            params![connector, crate::now_ms()],
        )?;
        Ok(())
    }

    pub fn get(&self, connector: &str) -> Result<Option<ConnectorHealth>> {
        Ok(self
            .db
            .conn()
            .query_row("SELECT connector, status, agent_version, capabilities, last_seen, last_error FROM connector_health WHERE connector = ?1", [connector], |r| {
                Ok(ConnectorHealth {
                    connector: r.get(0)?,
                    status: r.get(1)?,
                    agent_version: r.get(2)?,
                    capabilities: r.get::<_, Option<String>>(3)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null),
                    last_seen: r.get(4)?,
                    last_error: r.get(5)?,
                })
            })
            .optional()?)
    }
}
