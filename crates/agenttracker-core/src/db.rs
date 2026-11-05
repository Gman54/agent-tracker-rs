//! SQLite database handle and the versioned, embedded migration runner.
use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Ordered list of embedded migrations. Append new files here; never edit an applied one.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("v001", include_str!("../../../migrations/v001_initial.sql")),
    ("v002", include_str!("../../../migrations/v002_check_templates.sql")),
    ("v003", include_str!("../../../migrations/v003_event_extras_chain.sql")),
    ("v004", include_str!("../../../migrations/v004_runtime.sql")),
    ("v005", include_str!("../../../migrations/v005_auto_tasks.sql")),
];

pub struct Migrator;

impl Migrator {
    /// Applies all pending migrations (each inside its own transaction). Returns versions applied.
    pub fn run(conn: &Connection) -> Result<Vec<String>> {
        Self::run_up_to(conn, None)
    }

    /// Applies pending migrations up to and including `target` (or all if None).
    pub fn run_up_to(conn: &Connection, target: Option<&str>) -> Result<Vec<String>> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);")?;
        let mut applied = Vec::new();
        for (version, sql) in MIGRATIONS {
            let done: Option<String> = conn
                .query_row("SELECT version FROM schema_migrations WHERE version = ?1", [version], |r| r.get(0))
                .optional()?;
            if done.is_none() {
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(sql)?;
                tx.execute("INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)", params![version, crate::now_ms()])?;
                tx.commit()?;
                applied.push(version.to_string());
            }
            if Some(*version) == target {
                break;
            }
        }
        Ok(applied)
    }

    pub fn applied_versions(conn: &Connection) -> Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
        let v = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(v)
    }
}

/// Shared, thread-safe connection. All stores clone this handle.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        Migrator::run(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn shared(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Deletes one task and all data tied to it; returns the number of events removed.
    /// The tamper-evident chain keeps its redacted entries so later chain checks still verify.
    pub fn delete_task_data(&self, task_id: &str) -> Result<usize> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM manifest_entries WHERE manifest_id IN (SELECT id FROM manifests WHERE task_id = ?1)", [task_id])?;
        tx.execute("DELETE FROM manifests WHERE task_id = ?1", [task_id])?;
        tx.execute("DELETE FROM check_results WHERE run_id IN (SELECT id FROM verification_runs WHERE task_id = ?1)", [task_id])?;
        tx.execute("DELETE FROM verification_runs WHERE task_id = ?1", [task_id])?;
        let n = tx.execute("DELETE FROM events WHERE task_id = ?1", [task_id])?;
        for t in ["task_checks", "task_folders", "task_definition_snapshots"] {
            tx.execute(&format!("DELETE FROM {t} WHERE task_id = ?1"), [task_id])?;
        }
        tx.execute("DELETE FROM tasks WHERE id = ?1", [task_id])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn().query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}
