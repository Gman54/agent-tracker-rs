//! The engine's own SQLite database with versioned migrations. Never shared with client apps.
use rusqlite::Connection;
use std::path::Path;

const MIGRATIONS: &[(i64, &str)] = &[(
    1,
    r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE pairing_requests(
  id TEXT PRIMARY KEY, app_id TEXT NOT NULL, display_name TEXT NOT NULL, public_key TEXT NOT NULL,
  requested_scopes TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL, resolved_at INTEGER);
CREATE TABLE apps(
  app_id TEXT PRIMARY KEY, display_name TEXT NOT NULL, public_key TEXT NOT NULL, scopes TEXT NOT NULL,
  paired_at INTEGER NOT NULL, revoked_at INTEGER);
CREATE TABLE resource_grants(
  id TEXT PRIMARY KEY, app_id TEXT NOT NULL, folder TEXT NOT NULL, created_at INTEGER NOT NULL, revoked_at INTEGER);
CREATE TABLE policy_revisions(
  profile TEXT NOT NULL, revision INTEGER NOT NULL, body TEXT NOT NULL, hash TEXT NOT NULL, created_at INTEGER NOT NULL,
  PRIMARY KEY(profile, revision));
CREATE TABLE sessions(
  id TEXT PRIMARY KEY, app_id TEXT NOT NULL, profile TEXT NOT NULL, policy_revision INTEGER NOT NULL,
  grant_id TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL, started_at INTEGER,
  updated_at INTEGER NOT NULL, action_count INTEGER NOT NULL DEFAULT 0);
CREATE TABLE actions(
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL, app_id TEXT NOT NULL, idempotency_key TEXT NOT NULL,
  operation_id TEXT NOT NULL, args_hash TEXT NOT NULL, args TEXT NOT NULL, decision TEXT NOT NULL,
  state TEXT NOT NULL, reason TEXT NOT NULL, error_code TEXT, policy_revision INTEGER NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, UNIQUE(session_id, idempotency_key));
CREATE TABLE approvals(
  id TEXT PRIMARY KEY, action_id TEXT NOT NULL, session_id TEXT NOT NULL, args_hash TEXT NOT NULL,
  policy_revision INTEGER NOT NULL, nonce TEXT NOT NULL, expires_at INTEGER NOT NULL, state TEXT NOT NULL,
  resolved_at INTEGER);
CREATE TABLE events(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT NOT NULL, session_id TEXT, action_id TEXT,
  ts INTEGER NOT NULL, type TEXT NOT NULL, source TEXT NOT NULL, policy_revision INTEGER, payload TEXT NOT NULL);
CREATE INDEX events_session ON events(session_id, seq);
CREATE TABLE auth_failures(app_id TEXT NOT NULL, ts INTEGER NOT NULL);
"#,
), (
    2,
    r#"
-- The staged file worker records what it actually ran.
ALTER TABLE actions ADD COLUMN executed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE actions ADD COLUMN output TEXT;
-- Where the session's snapshot/staging folders live, and the snapshot it took.
ALTER TABLE sessions ADD COLUMN snapshot_files INTEGER;
ALTER TABLE sessions ADD COLUMN snapshot_bytes INTEGER;
"#,
)];

pub fn open(dir: &Path) -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(dir).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let c = Connection::open(dir.join("protect-engine.db"))?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "FULL")?;
    let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (n, sql) in MIGRATIONS {
        if *n > v {
            let tx = c.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", n)?;
            tx.commit()?;
        }
    }
    Ok(c)
}
