-- v006: local Git tracking and optional read-only GitHub links.
-- Everything here stays on this computer. File snippets are redacted like event payloads.

-- Repositories the user picked to look at.
CREATE TABLE git_repos (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,          -- top-level folder as reported by git
    identity TEXT NOT NULL,             -- normalized remote URL, or "local:<first commit id>"
    remote_url TEXT,
    github_owner TEXT,
    github_repo TEXT,
    github_enabled INTEGER NOT NULL DEFAULT 0,
    added_at INTEGER NOT NULL
);

-- Before-state captured when an agent announces an edit (PreToolUse), before the file changes.
CREATE TABLE git_pre_snapshots (
    tool_call_id TEXT PRIMARY KEY,
    abs_path TEXT NOT NULL,
    existed INTEGER NOT NULL,
    content TEXT,                       -- redacted text; NULL when binary or too large
    blob TEXT,                          -- git blob id of the exact bytes
    captured_at INTEGER NOT NULL
);

-- One row per file per agent decision that edited a file inside a Git repository.
CREATE TABLE git_agent_edits (
    id TEXT PRIMARY KEY,
    decision_id TEXT NOT NULL,          -- tool call id (hooks) or rt_decisions.id (runtime)
    decision_source TEXT NOT NULL,      -- hook_event | runtime_decision
    event_id TEXT,
    session_id TEXT,
    task_id TEXT,
    connector TEXT NOT NULL,
    tool TEXT NOT NULL,
    repo_root TEXT NOT NULL,
    repo_identity TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    proposal TEXT NOT NULL,             -- JSON array of {old, new} the agent asked for (redacted)
    before_source TEXT NOT NULL,        -- pre_hook_snapshot | agent_report | none
    before_file TEXT,
    before_blob TEXT,
    after_file TEXT,                    -- file as read right after the edit (redacted), NULL if unreadable
    after_blob TEXT,
    head_at_edit TEXT,
    branch_at_edit TEXT,
    user_modified INTEGER,              -- agent reported the user changed the proposal (NULL = not reported)
    observed_at INTEGER NOT NULL,
    UNIQUE (decision_id, rel_path)
);
CREATE INDEX git_edits_repo ON git_agent_edits(repo_identity, rel_path, observed_at);

-- git commit / git push commands an agent ran (seen through its shell tool).
CREATE TABLE git_command_facts (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL,
    session_id TEXT,
    connector TEXT NOT NULL,
    kind TEXT NOT NULL,                 -- commit | push
    repo_root TEXT NOT NULL,
    repo_identity TEXT NOT NULL,
    command TEXT NOT NULL,              -- redacted, shortened
    succeeded INTEGER,                  -- NULL = the agent didn't say
    head_after TEXT,
    branch_after TEXT,
    observed_at INTEGER NOT NULL,
    UNIQUE (event_id, kind)
);
CREATE INDEX git_cmds_repo ON git_command_facts(repo_identity, observed_at);
