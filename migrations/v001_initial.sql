-- v001: initial AgentTracker schema
CREATE TABLE IF NOT EXISTS schema_migrations (
    version TEXT PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    connector TEXT NOT NULL,
    agent_version TEXT,
    working_dir TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    state TEXT NOT NULL DEFAULT 'active'
);

CREATE TABLE events (
    id TEXT PRIMARY KEY,
    schema_version TEXT NOT NULL,
    connector TEXT NOT NULL,
    session_id TEXT,
    task_id TEXT,
    tool_call_id TEXT,
    event_type TEXT NOT NULL,
    source_ts INTEGER,
    collector_ts INTEGER NOT NULL,
    source_seq INTEGER,
    working_dir TEXT,
    payload_redacted TEXT NOT NULL,
    capture_source TEXT NOT NULL,
    raw_source TEXT,
    chain_seq INTEGER
);
CREATE INDEX idx_events_session ON events(session_id, source_seq);
CREATE INDEX idx_events_task ON events(task_id);

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    instruction TEXT,
    definition_version INTEGER NOT NULL DEFAULT 1,
    lifecycle TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE task_folders (
    task_id TEXT NOT NULL,
    folder_path TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('working', 'protected', 'allowed_write'))
);
CREATE INDEX idx_task_folders ON task_folders(task_id);

CREATE TABLE task_checks (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    check_type TEXT NOT NULL,
    params TEXT NOT NULL,
    is_required INTEGER NOT NULL DEFAULT 1,
    definition_version INTEGER NOT NULL
);
CREATE INDEX idx_task_checks ON task_checks(task_id, definition_version);

CREATE TABLE manifests (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    folder_path TEXT NOT NULL,
    captured_at INTEGER NOT NULL,
    manifest_type TEXT NOT NULL CHECK (manifest_type IN ('baseline', 'verification'))
);
CREATE INDEX idx_manifests_task ON manifests(task_id, manifest_type);

CREATE TABLE manifest_entries (
    manifest_id TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    entry_type TEXT NOT NULL,
    size_bytes INTEGER,
    sha256 TEXT,
    modified_ts INTEGER,
    observed_ts INTEGER,
    read_ok INTEGER NOT NULL,
    error_detail TEXT
);
CREATE INDEX idx_manifest_entries ON manifest_entries(manifest_id);

CREATE TABLE verification_runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    definition_version INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    overall_verdict TEXT
);

CREATE TABLE check_results (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    check_id TEXT NOT NULL,
    check_type TEXT NOT NULL,
    check_version TEXT NOT NULL,
    status TEXT NOT NULL,
    summary TEXT NOT NULL,
    evidence TEXT,
    limitations TEXT,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX idx_check_results_run ON check_results(run_id);

CREATE TABLE connector_health (
    connector TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    agent_version TEXT,
    capabilities TEXT,
    last_seen INTEGER,
    last_error TEXT
);

CREATE TABLE queue (
    id TEXT PRIMARY KEY,
    connector TEXT NOT NULL,
    payload TEXT NOT NULL,
    enqueued_at INTEGER NOT NULL,
    delivered INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_queue_pending ON queue(delivered, enqueued_at);
