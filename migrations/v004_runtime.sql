-- v004: instrumented decision runtime (agenttracker-runtime).
-- Accepted records (context items, proposal attempts, decisions, actions, action states, results,
-- observed changes, findings, replay trials) are append-only at the application level: triggers
-- reject UPDATE/DELETE. Corrections are new rows linked to the originals.
-- This does not stop a process with write access to the database file from rewriting it.

CREATE TABLE rt_tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    instruction TEXT NOT NULL,
    working_dir TEXT NOT NULL,
    protected_paths TEXT NOT NULL,      -- JSON array of relative paths/prefixes
    definition_version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_sessions (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES rt_tasks(id),
    parent_session_id TEXT,
    idempotency_key TEXT UNIQUE,
    working_dir TEXT NOT NULL,
    provider_config TEXT NOT NULL,      -- JSON, API key never stored here
    limits TEXT NOT NULL,               -- JSON
    status TEXT NOT NULL,               -- running | paused | awaiting_approval | completed | failed
    status_reason TEXT,
    steps_used INTEGER NOT NULL DEFAULT 0,
    tool_calls_used INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Every piece of information the runtime may place in a model request. Labels (instruction_3,
-- evidence_7) are the stable references exposed to the model; ids are authoritative.
CREATE TABLE rt_context_items (
    id TEXT PRIMARY KEY,                -- instruction_id or evidence_id
    session_id TEXT NOT NULL REFERENCES rt_sessions(id),
    label TEXT NOT NULL,
    kind TEXT NOT NULL,                 -- instruction | evidence
    role TEXT NOT NULL,                 -- system | developer | user | tool_result | retrieved | assistant
    trust TEXT NOT NULL,                -- application | user | tool_result | retrieved_document | model_output
    source TEXT NOT NULL,               -- human-readable source description
    source_ref TEXT,                    -- e.g. file path, action id
    is_excerpt INTEGER NOT NULL DEFAULT 0,
    excerpt_range TEXT,
    content_redacted TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,       -- hash of the content actually sent (pre-redaction)
    redaction_applied INTEGER NOT NULL,
    produced_by_action_id TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (session_id, label)
);

CREATE TABLE rt_context_snapshots (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES rt_sessions(id),
    step INTEGER NOT NULL,
    tool_definitions TEXT NOT NULL,
    compaction TEXT NOT NULL,           -- JSON: items dropped for the context limit, truncations
    omitted TEXT NOT NULL,              -- JSON: information known to be unavailable to the model
    rendered_sha256 TEXT NOT NULL,      -- hash of the exact messages payload sent
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_snapshot_items (
    snapshot_id TEXT NOT NULL REFERENCES rt_context_snapshots(id),
    item_id TEXT NOT NULL REFERENCES rt_context_items(id),
    position INTEGER NOT NULL,
    included INTEGER NOT NULL,          -- 0 = known to the session but dropped from this request
    PRIMARY KEY (snapshot_id, item_id)
);

CREATE TABLE rt_model_requests (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES rt_sessions(id),
    snapshot_id TEXT NOT NULL REFERENCES rt_context_snapshots(id),
    step INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    purpose TEXT NOT NULL,              -- decision | repair
    provider TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    model TEXT NOT NULL,
    model_digest TEXT,
    settings TEXT NOT NULL,
    runtime_version TEXT NOT NULL,
    structured_output_mode TEXT NOT NULL,
    payload_redacted TEXT NOT NULL,     -- application-level request body, redacted
    response_redacted TEXT,
    provider_reasoning_redacted TEXT,   -- "provider-exposed reasoning", optional
    status TEXT NOT NULL,               -- ok | transport_error
    error TEXT,
    replay_captured INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE rt_proposal_attempts (
    id TEXT PRIMARY KEY,
    model_request_id TEXT NOT NULL REFERENCES rt_model_requests(id),
    session_id TEXT NOT NULL,
    step INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    raw_redacted TEXT,
    valid INTEGER NOT NULL,
    errors TEXT NOT NULL,               -- JSON array of {code, message}
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_decisions (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES rt_sessions(id),
    model_request_id TEXT NOT NULL REFERENCES rt_model_requests(id),
    snapshot_id TEXT NOT NULL REFERENCES rt_context_snapshots(id),
    attempt_id TEXT NOT NULL REFERENCES rt_proposal_attempts(id),
    step INTEGER NOT NULL,
    kind TEXT NOT NULL,                 -- action | response
    proposal TEXT NOT NULL,             -- validated proposal JSON (redacted)
    instruction_ids TEXT NOT NULL,
    evidence_ids TEXT NOT NULL,
    tool TEXT,
    args TEXT,
    args_sha256 TEXT,
    approval_required INTEGER NOT NULL DEFAULT 0,
    revises_decision_id TEXT REFERENCES rt_decisions(id),
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_approvals (
    id TEXT PRIMARY KEY,
    decision_id TEXT NOT NULL UNIQUE REFERENCES rt_decisions(id),
    approved_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_actions (
    id TEXT PRIMARY KEY,
    decision_id TEXT NOT NULL UNIQUE REFERENCES rt_decisions(id),
    session_id TEXT NOT NULL,
    model_request_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    args TEXT NOT NULL,
    args_sha256 TEXT NOT NULL,
    approval_ref TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_action_states (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    action_id TEXT NOT NULL REFERENCES rt_actions(id),
    state TEXT NOT NULL,                -- prepared | started | succeeded | failed | unknown
    detail TEXT,
    at INTEGER NOT NULL
);

CREATE TABLE rt_results (
    id TEXT PRIMARY KEY,
    action_id TEXT NOT NULL UNIQUE REFERENCES rt_actions(id),
    status TEXT NOT NULL,               -- succeeded | failed
    output_redacted TEXT,
    error TEXT,
    evidence_id TEXT,
    finished_at INTEGER NOT NULL
);

-- Independent observation of the working directory around an action (not the tool's own report).
CREATE TABLE rt_verifications (
    id TEXT PRIMARY KEY,                -- verification_id
    action_id TEXT NOT NULL REFERENCES rt_actions(id),
    session_id TEXT NOT NULL,
    method TEXT NOT NULL,               -- manifest_diff | reconciliation
    summary TEXT NOT NULL,
    detail TEXT NOT NULL,               -- JSON
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_changes (
    id TEXT PRIMARY KEY,
    verification_id TEXT REFERENCES rt_verifications(id),
    action_id TEXT REFERENCES rt_actions(id),
    session_id TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    change_kind TEXT NOT NULL,          -- added | modified | removed
    before_sha256 TEXT,
    after_sha256 TEXT,
    observed_at INTEGER NOT NULL
);

CREATE TABLE rt_findings (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    decision_id TEXT,
    action_id TEXT,
    check_type TEXT NOT NULL,
    basis TEXT NOT NULL,                -- deterministic | inferred
    summary TEXT NOT NULL,
    excerpts TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE rt_investigations (
    id TEXT PRIMARY KEY,
    action_id TEXT,
    decision_id TEXT NOT NULL,
    model_request_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    context_item_id TEXT NOT NULL,
    modification TEXT NOT NULL,
    settings TEXT NOT NULL,
    trials_per_arm INTEGER NOT NULL,
    comparison_method TEXT NOT NULL,
    status TEXT NOT NULL,               -- completed | not_replayable | failed
    replay_fidelity TEXT NOT NULL,      -- exact_application_payload | modified_redacted | unavailable
    confounded INTEGER NOT NULL DEFAULT 0,
    confound_detail TEXT,
    summary TEXT,
    counts TEXT,
    limitations TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE rt_replay_trials (
    id TEXT PRIMARY KEY,
    investigation_id TEXT NOT NULL REFERENCES rt_investigations(id),
    arm TEXT NOT NULL,                  -- baseline | modified
    trial_no INTEGER NOT NULL,
    seed INTEGER,
    proposal TEXT,
    normalized_action TEXT,
    valid INTEGER NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
);

-- Optional, explicitly enabled replay capture: exact (unredacted) request payloads, encrypted.
-- Deletable; not part of the append-only audit record.
CREATE TABLE rt_replay_store (
    model_request_id TEXT PRIMARY KEY,
    nonce TEXT NOT NULL,
    ciphertext TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX rt_items_session ON rt_context_items(session_id);
CREATE INDEX rt_decisions_session ON rt_decisions(session_id);
CREATE INDEX rt_actions_session ON rt_actions(session_id);
CREATE INDEX rt_states_action ON rt_action_states(action_id);
CREATE INDEX rt_changes_action ON rt_changes(action_id);

CREATE TRIGGER rt_items_no_update BEFORE UPDATE ON rt_context_items BEGIN SELECT RAISE(ABORT, 'append-only: rt_context_items'); END;
CREATE TRIGGER rt_items_no_delete BEFORE DELETE ON rt_context_items BEGIN SELECT RAISE(ABORT, 'append-only: rt_context_items'); END;
CREATE TRIGGER rt_attempts_no_update BEFORE UPDATE ON rt_proposal_attempts BEGIN SELECT RAISE(ABORT, 'append-only: rt_proposal_attempts'); END;
CREATE TRIGGER rt_attempts_no_delete BEFORE DELETE ON rt_proposal_attempts BEGIN SELECT RAISE(ABORT, 'append-only: rt_proposal_attempts'); END;
CREATE TRIGGER rt_decisions_no_update BEFORE UPDATE ON rt_decisions BEGIN SELECT RAISE(ABORT, 'append-only: rt_decisions'); END;
CREATE TRIGGER rt_decisions_no_delete BEFORE DELETE ON rt_decisions BEGIN SELECT RAISE(ABORT, 'append-only: rt_decisions'); END;
CREATE TRIGGER rt_actions_no_update BEFORE UPDATE ON rt_actions BEGIN SELECT RAISE(ABORT, 'append-only: rt_actions'); END;
CREATE TRIGGER rt_actions_no_delete BEFORE DELETE ON rt_actions BEGIN SELECT RAISE(ABORT, 'append-only: rt_actions'); END;
CREATE TRIGGER rt_states_no_update BEFORE UPDATE ON rt_action_states BEGIN SELECT RAISE(ABORT, 'append-only: rt_action_states'); END;
CREATE TRIGGER rt_states_no_delete BEFORE DELETE ON rt_action_states BEGIN SELECT RAISE(ABORT, 'append-only: rt_action_states'); END;
CREATE TRIGGER rt_results_no_update BEFORE UPDATE ON rt_results BEGIN SELECT RAISE(ABORT, 'append-only: rt_results'); END;
CREATE TRIGGER rt_results_no_delete BEFORE DELETE ON rt_results BEGIN SELECT RAISE(ABORT, 'append-only: rt_results'); END;
CREATE TRIGGER rt_changes_no_update BEFORE UPDATE ON rt_changes BEGIN SELECT RAISE(ABORT, 'append-only: rt_changes'); END;
CREATE TRIGGER rt_changes_no_delete BEFORE DELETE ON rt_changes BEGIN SELECT RAISE(ABORT, 'append-only: rt_changes'); END;
CREATE TRIGGER rt_approvals_no_update BEFORE UPDATE ON rt_approvals BEGIN SELECT RAISE(ABORT, 'append-only: rt_approvals'); END;
CREATE TRIGGER rt_approvals_no_delete BEFORE DELETE ON rt_approvals BEGIN SELECT RAISE(ABORT, 'append-only: rt_approvals'); END;
CREATE TRIGGER rt_verif_no_update BEFORE UPDATE ON rt_verifications BEGIN SELECT RAISE(ABORT, 'append-only: rt_verifications'); END;
CREATE TRIGGER rt_verif_no_delete BEFORE DELETE ON rt_verifications BEGIN SELECT RAISE(ABORT, 'append-only: rt_verifications'); END;
CREATE TRIGGER rt_findings_no_update BEFORE UPDATE ON rt_findings BEGIN SELECT RAISE(ABORT, 'append-only: rt_findings'); END;
CREATE TRIGGER rt_findings_no_delete BEFORE DELETE ON rt_findings BEGIN SELECT RAISE(ABORT, 'append-only: rt_findings'); END;
CREATE TRIGGER rt_trials_no_update BEFORE UPDATE ON rt_replay_trials BEGIN SELECT RAISE(ABORT, 'append-only: rt_replay_trials'); END;
CREATE TRIGGER rt_trials_no_delete BEFORE DELETE ON rt_replay_trials BEGIN SELECT RAISE(ABORT, 'append-only: rt_replay_trials'); END;
