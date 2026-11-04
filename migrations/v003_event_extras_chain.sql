-- v003: remaining normalized event fields, hash chain, definition snapshots, manifest stability,
-- task connector selection and key/value settings.
ALTER TABLE events ADD COLUMN parent_id TEXT;
ALTER TABLE events ADD COLUMN connector_version TEXT;
ALTER TABLE events ADD COLUMN agent_version TEXT;
ALTER TABLE events ADD COLUMN collector_generated INTEGER NOT NULL DEFAULT 0;

ALTER TABLE manifest_entries ADD COLUMN stability TEXT NOT NULL DEFAULT 'stable';

ALTER TABLE tasks ADD COLUMN connector TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE tasks ADD COLUMN session_id TEXT;

CREATE TABLE IF NOT EXISTS chain_entries (
    seq INTEGER PRIMARY KEY,
    event_id TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    event_hash TEXT NOT NULL,
    chain_hash TEXT NOT NULL,
    ts INTEGER NOT NULL,
    event_canonical TEXT NOT NULL
);

CREATE TABLE task_definition_snapshots (
    task_id TEXT NOT NULL,
    definition_version INTEGER NOT NULL,
    snapshot TEXT NOT NULL,
    frozen_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, definition_version)
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
