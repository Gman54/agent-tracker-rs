-- Quick name list taken the moment an agent prompt arrives (names, sizes, modified times; no
-- hashing), so a fast deletion before the full "before" snapshot reaches a file is still caught.
CREATE TABLE quick_name_lists (
    id TEXT PRIMARY KEY,
    queue_id TEXT,
    task_id TEXT,
    root TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    file_count INTEGER NOT NULL DEFAULT 0,
    cut_short INTEGER NOT NULL DEFAULT 0,
    cut_reason TEXT,
    error TEXT,
    entries TEXT
);
CREATE INDEX idx_quick_name_lists_queue ON quick_name_lists(queue_id);
CREATE INDEX idx_quick_name_lists_task ON quick_name_lists(task_id);

-- When the full "before" snapshot of a task started and finished.
CREATE TABLE task_snapshot_times (
    task_id TEXT PRIMARY KEY,
    baseline_started_at INTEGER,
    baseline_finished_at INTEGER
);
