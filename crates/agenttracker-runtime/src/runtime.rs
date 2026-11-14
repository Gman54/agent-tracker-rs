//! The decision runtime: context capture → model request → proposal validation → durable decision →
//! (approval) → confined execution → linked result, independent observation and findings.
use crate::checks;
use crate::config::RuntimeConfig;
use crate::context::{self, ContextItem, Fidelity};
use crate::ids::{self, new_id};
use crate::proposal::{self, ValidationCtx};
use crate::provider::{ChatMessage, ChatRequest, Provider};
use crate::replay_vault::ReplayVault;
use crate::store::{self, insert};
use crate::tools;
use crate::{now_ms, sha256_hex, Result, RtError, RUNTIME_VERSION};
use agenttracker_core::db::Db;
use agenttracker_core::fs_observer::Snapshotter;
use once_cell_lite::Lazy;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracker_common::canonical::canonical_json;
use tracker_common::hash_chain::ChainStore;
use tracker_common::redaction::{redact_deep, redact_text};

mod once_cell_lite {
    pub struct Lazy<T>(std::sync::OnceLock<T>, fn() -> T);
    impl<T> Lazy<T> {
        pub const fn new(f: fn() -> T) -> Self {
            Self(std::sync::OnceLock::new(), f)
        }
        pub fn get(&self) -> &T {
            self.0.get_or_init(self.1)
        }
    }
}

/// Sessions currently being driven in this process (prevents duplicate concurrent runs).
static RUNNING: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
/// Exact content of context items whose audit copy was redacted (process lifetime only).
static EXACT: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// Exact arguments of accepted decisions whose stored copy was redacted (process lifetime only).
static EXACT_ARGS: Lazy<Mutex<HashMap<String, Value>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocSpec {
    /// Path relative to the working folder.
    pub path: String,
    /// Trust label recorded with the evidence, e.g. "retrieved_document".
    #[serde(default = "d_trust")]
    pub trust: String,
    #[serde(default = "d_doc_chars")]
    pub max_chars: usize,
}

fn d_trust() -> String {
    "retrieved_document".into()
}
fn d_doc_chars() -> usize {
    4000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    pub instruction: String,
    pub working_dir: PathBuf,
    #[serde(default)]
    pub protected_paths: Vec<String>,
    #[serde(default)]
    pub documents: Vec<DocSpec>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartOutcome {
    pub task_id: String,
    pub session_id: String,
    /// true when the idempotency key matched an existing session (nothing new was started).
    pub existing: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionStatus {
    pub session_id: String,
    pub status: String,
    pub reason: Option<String>,
}

enum StepOutcome {
    Action,
    Response,
    Clarify(String),
    Paused(String),
}

pub fn args_sha(args: &Value) -> String {
    sha256_hex(canonical_json(args).as_bytes())
}

pub struct Runtime {
    pub db: Db,
    pub provider: Arc<dyn Provider>,
    pub cfg: RuntimeConfig,
    pub vault: Option<Arc<ReplayVault>>,
    /// Fault injection for crash-recovery tests: stop right after recording `started`.
    pub crash_after_start: bool,
}

struct RunGuard(String);
impl Drop for RunGuard {
    fn drop(&mut self) {
        lock(RUNNING.get()).remove(&self.0);
    }
}

impl Runtime {
    pub fn new(db: Db, provider: Arc<dyn Provider>, cfg: RuntimeConfig, vault: Option<Arc<ReplayVault>>) -> Self {
        Self { db, provider, cfg, vault, crash_after_start: false }
    }

    // ---- sessions ------------------------------------------------------------------------------------
    pub fn start_task(&self, spec: &TaskSpec) -> Result<StartOutcome> {
        if spec.instruction.trim().is_empty() {
            return Err(RtError::Invalid("Describe the task".into()));
        }
        let root = spec.working_dir.canonicalize().map_err(|_| RtError::Invalid("Working folder does not exist".into()))?;
        if !root.is_dir() {
            return Err(RtError::Invalid("Working folder is not a folder".into()));
        }
        let conn = self.db.conn();
        if let Some(k) = spec.idempotency_key.as_deref() {
            let found: Option<(String, String)> = conn.query_row("SELECT id, task_id FROM rt_sessions WHERE idempotency_key=?1", [k], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if let Some((sid, tid)) = found {
                return Ok(StartOutcome { task_id: tid, session_id: sid, existing: true });
            }
        }
        let tx = conn.unchecked_transaction()?;
        let now = now_ms();
        let task_id = new_id(ids::TASK);
        let session_id = new_id(ids::SESSION);
        let root_s = root.to_string_lossy().into_owned();
        insert(&tx, "rt_tasks", json!({"id": task_id, "title": spec.title, "instruction": redact_text(&spec.instruction), "working_dir": root_s, "protected_paths": spec.protected_paths, "definition_version": 1, "created_at": now}))?;
        let mut pc = serde_json::to_value(self.provider.info())?;
        pc["settings"] = self.provider.settings();
        insert(&tx, "rt_sessions", json!({"id": session_id, "task_id": task_id, "parent_session_id": spec.parent_session_id, "idempotency_key": spec.idempotency_key,
            "working_dir": root_s, "provider_config": pc, "limits": self.cfg.limits, "status": "running", "status_reason": null, "steps_used": 0, "tool_calls_used": 0, "created_at": now, "updated_at": now}))?;
        let sys = ContextItem::new(new_id(ids::INSTRUCTION), "instruction_1".into(), "instruction", "system", "application", "Houndbit runtime system instruction", None, context::SYSTEM_INSTRUCTION.into());
        add_item(&tx, &session_id, sys)?;
        let user = ContextItem::new(new_id(ids::INSTRUCTION), "instruction_2".into(), "instruction", "user", "user", "Task request typed by the user", Some(task_id.clone()), spec.instruction.clone());
        add_item(&tx, &session_id, user)?;
        if !spec.protected_paths.is_empty() {
            let text = format!("These paths are protected and must not be modified: {}", spec.protected_paths.join(", "));
            add_item(&tx, &session_id, ContextItem::new(new_id(ids::INSTRUCTION), "instruction_3".into(), "instruction", "user", "user", "Protected-path policy set by the user", None, text))?;
        }
        for d in &spec.documents {
            let label = next_label(&tx, &session_id, "evidence")?;
            let path = tools::resolve(&root, &d.path).map_err(|e| RtError::Invalid(e.message("document")))?;
            let full = std::fs::read_to_string(&path).map_err(|e| RtError::Invalid(format!("cannot read document {}: {e}", d.path)))?;
            let cut: String = full.chars().take(d.max_chars).collect();
            let is_excerpt = cut.len() < full.len();
            let mut it = ContextItem::new(new_id(ids::EVIDENCE), label, "evidence", "retrieved", &d.trust, &format!("Document {}", d.path), Some(d.path.clone()), cut.clone());
            it.is_excerpt = is_excerpt;
            it.excerpt_range = Some(if is_excerpt { format!("chars 0..{} of {}", cut.chars().count(), full.chars().count()) } else { "full document".into() });
            add_item(&tx, &session_id, it)?;
        }
        tx.commit()?;
        Ok(StartOutcome { task_id, session_id, existing: false })
    }

    pub fn status(&self, session_id: &str) -> Result<SessionStatus> {
        let conn = self.db.conn();
        let (status, reason): (String, Option<String>) = conn
            .query_row("SELECT status, status_reason FROM rt_sessions WHERE id=?1", [session_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .ok_or_else(|| RtError::NotFound(session_id.into()))?;
        Ok(SessionStatus { session_id: session_id.into(), status, reason })
    }

    fn set_status(&self, session_id: &str, status: &str, reason: Option<&str>) -> Result<()> {
        let conn = self.db.conn();
        conn.execute("UPDATE rt_sessions SET status=?2, status_reason=?3, updated_at=?4 WHERE id=?1", params![session_id, status, reason, now_ms()])?;
        ChainStore::append_with(&conn, &format!("{session_id}#status"), &json!({"rt_table": "rt_sessions", "status": status, "reason": reason}), now_ms())?;
        Ok(())
    }

    /// Drives a session until it completes, pauses, needs approval or hits a limit.
    pub fn run(&self, session_id: &str) -> Result<SessionStatus> {
        if !lock(RUNNING.get()).insert(session_id.to_string()) {
            return Err(RtError::Refused("this session is already running".into()));
        }
        let _g = RunGuard(session_id.to_string());
        loop {
            let st = self.status(session_id)?;
            if st.status != "running" && st.status != "awaiting_approval" {
                return Ok(st);
            }
            let (pending, unknown, steps, (wd, prot)) = {
                let conn = self.db.conn();
                let pending: Option<(String, i64)> = conn
                    .query_row(
                        "SELECT d.id, d.approval_required FROM rt_decisions d LEFT JOIN rt_actions a ON a.decision_id=d.id WHERE d.session_id=?1 AND d.kind='action' AND a.id IS NULL ORDER BY d.created_at DESC LIMIT 1",
                        [session_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let unknown: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM rt_actions a WHERE a.session_id=?1 AND (SELECT state FROM rt_action_states s WHERE s.action_id=a.id ORDER BY seq DESC LIMIT 1) IN ('unknown','started','prepared')",
                    [session_id],
                    |r| r.get(0),
                )?;
                let steps: i64 = conn.query_row("SELECT steps_used FROM rt_sessions WHERE id=?1", [session_id], |r| r.get(0))?;
                (pending, unknown, steps, session_paths(&conn, session_id)?)
            };
            if unknown > 0 {
                self.set_status(session_id, "paused", Some("An earlier action has an unknown or unfinished outcome. Review it before continuing; it will not be repeated automatically."))?;
                continue;
            }
            if let Some((dec_id, approval_required)) = pending {
                if approval_required != 0 && !self.has_approval(&dec_id)? {
                    self.set_status(session_id, "awaiting_approval", Some("A proposed action needs your approval before it runs."))?;
                    return self.status(session_id);
                }
                if st.status == "awaiting_approval" {
                    self.set_status(session_id, "running", None)?;
                }
                let args = self.exact_args(&dec_id)?;
                match self.execute_decision(&dec_id, &args) {
                    Ok(_) => continue,
                    Err(RtError::Refused(m)) => {
                        self.set_status(session_id, "paused", Some(&m))?;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            if steps as u32 >= self.cfg.limits.max_steps {
                self.set_status(session_id, "paused", Some(&format!("Step limit reached ({} steps).", self.cfg.limits.max_steps)))?;
                continue;
            }
            let step = steps as u32 + 1;
            self.db.conn().execute("UPDATE rt_sessions SET steps_used=?2, updated_at=?3 WHERE id=?1", params![session_id, step, now_ms()])?;
            match self.decide(session_id, step, Path::new(&wd), &prot)? {
                StepOutcome::Action => continue,
                StepOutcome::Response => self.set_status(session_id, "completed", Some("The model returned a final response without a tool action."))?,
                StepOutcome::Clarify(q) => self.set_status(session_id, "paused", Some(&format!("The model asked for clarification: {q}")))?,
                StepOutcome::Paused(r) => self.set_status(session_id, "paused", Some(&r))?,
            }
        }
    }

    fn has_approval(&self, decision_id: &str) -> Result<bool> {
        Ok(self.db.conn().query_row("SELECT COUNT(*) FROM rt_approvals WHERE decision_id=?1", [decision_id], |r| r.get::<_, i64>(0))? > 0)
    }

    pub fn approve(&self, decision_id: &str, approved_by: &str) -> Result<String> {
        let conn = self.db.conn();
        let exists: i64 = conn.query_row("SELECT COUNT(*) FROM rt_decisions WHERE id=?1 AND kind='action'", [decision_id], |r| r.get(0))?;
        if exists == 0 {
            return Err(RtError::NotFound(decision_id.into()));
        }
        if let Some(id) = conn.query_row("SELECT id FROM rt_approvals WHERE decision_id=?1", [decision_id], |r| r.get::<_, String>(0)).optional()? {
            return Ok(id);
        }
        let id = new_id(ids::APPROVAL);
        let tx = conn.unchecked_transaction()?;
        insert(&tx, "rt_approvals", json!({"id": id, "decision_id": decision_id, "approved_by": approved_by, "created_at": now_ms()}))?;
        tx.commit()?;
        Ok(id)
    }

    fn exact_args(&self, decision_id: &str) -> Result<Value> {
        if let Some(a) = lock(EXACT_ARGS.get()).get(decision_id) {
            return Ok(a.clone());
        }
        let conn = self.db.conn();
        let (args, sha): (String, String) = conn.query_row("SELECT args, args_sha256 FROM rt_decisions WHERE id=?1", [decision_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let v: Value = serde_json::from_str(&args)?;
        if args_sha(&v) != sha {
            return Err(RtError::Refused("The exact arguments of this decision were redacted and are no longer in memory; a new decision is required.".into()));
        }
        Ok(v)
    }

    // ---- one decision step -----------------------------------------------------------------------------
    fn decide(&self, session_id: &str, step: u32, root: &Path, protected: &[String]) -> Result<StepOutcome> {
        let (items, unavailable_exact) = load_items(&self.db.conn(), session_id)?;
        let sel = context::select(&items, self.provider_budget());
        let snapshot_id = new_id(ids::SNAPSHOT);
        let msgs = context::render(&sel.included, step, Fidelity::Exact);
        let mut omitted: Vec<Value> = sel.dropped.iter().map(|d| json!({"label": d.label, "item_id": d.id, "reason": "dropped for the context budget"})).collect();
        for (label, id) in &unavailable_exact {
            if sel.included.iter().any(|i| &i.id == id) {
                omitted.push(json!({"label": label, "item_id": id, "reason": "exact content not in memory after restart; the redacted audit copy was sent instead"}));
            }
        }
        omitted.push(json!({"reason": "Files in the working folder that were not listed or read are not visible to the model."}));
        omitted.push(json!({"reason": "Provider-side chat templates, hidden prompts, tokenisation and model state are not captured."}));
        {
            let conn = self.db.conn();
            let tx = conn.unchecked_transaction()?;
            insert(&tx, "rt_context_snapshots", json!({"id": snapshot_id, "session_id": session_id, "step": step, "tool_definitions": tools::definitions_json(), "compaction": sel.compaction, "omitted": omitted, "rendered_sha256": context::messages_sha(&msgs), "created_at": now_ms()}))?;
            for (pos, it) in items.iter().enumerate() {
                let included = sel.included.iter().any(|i| i.id == it.id);
                tx.execute("INSERT INTO rt_snapshot_items(snapshot_id,item_id,position,included) VALUES (?1,?2,?3,?4)", params![snapshot_id, it.id, pos as i64, included as i64])?;
            }
            tx.commit()?;
        }
        let redacted_msgs = context::render(&sel.included, step, Fidelity::Redacted);
        let redaction_changed = redacted_msgs != msgs;
        let tool_calls_used: i64 = self.db.conn().query_row("SELECT tool_calls_used FROM rt_sessions WHERE id=?1", [session_id], |r| r.get(0))?;
        let ctx = ValidationCtx {
            included: sel.included.iter().map(|i| (i.label.clone(), (i.id.clone(), i.kind.clone()))).collect(),
            known: items.iter().map(|i| i.label.clone()).collect(),
            root,
            protected,
            tool_calls_remaining: self.cfg.limits.max_tool_calls.saturating_sub(tool_calls_used as u32),
        };
        let info = self.provider.info();
        let mut repair: Vec<ChatMessage> = Vec::new();
        let mut last_errors = String::new();
        for attempt in 0..=self.cfg.limits.max_validation_retries {
            let mut req_msgs = msgs.clone();
            req_msgs.extend(repair.iter().cloned());
            let mut red_req = redacted_msgs.clone();
            red_req.extend(repair.iter().map(|m| ChatMessage { role: m.role.clone(), content: redact_text(&m.content) }));
            let req_id = new_id(ids::MODEL_REQUEST);
            let payload = json!({
                "protocol": context::PROTOCOL_VERSION, "messages": red_req, "response_schema": proposal::schema(),
                "context_labels": sel.included.iter().map(|i| &i.label).collect::<Vec<_>>(),
                "exact_messages_sha256": context::messages_sha(&req_msgs),
                "redaction_changed_payload": redaction_changed || red_req.iter().zip(&req_msgs).any(|(a, b)| a != b),
                "capture_scope": "application-level payload only",
                "prompt_chars": req_msgs.iter().map(|m| m.content.chars().count()).sum::<usize>(),
            });
            let captured = if self.cfg.replay_capture { self.vault.is_some() } else { false };
            {
                let conn = self.db.conn();
                let tx = conn.unchecked_transaction()?;
                insert(&tx, "rt_model_requests", json!({"id": req_id, "session_id": session_id, "snapshot_id": snapshot_id, "step": step, "attempt": attempt, "purpose": if attempt == 0 { "decision" } else { "repair" },
                    "provider": info.provider, "endpoint": info.endpoint, "model": info.model, "model_digest": info.model_digest, "settings": self.provider.settings(), "runtime_version": RUNTIME_VERSION,
                    "structured_output_mode": "pending", "payload_redacted": payload, "status": "sent", "replay_captured": captured, "started_at": now_ms()}))?;
                if captured {
                    if let Some(v) = &self.vault {
                        v.store(&tx, &req_id, &json!({"messages": req_msgs, "items": sel.included.iter().map(|i| json!({"id": i.id, "label": i.label, "content": i.content})).collect::<Vec<_>>()}), self.cfg.replay_retention_days)?;
                    }
                }
                tx.commit()?;
            }
            let resp = self.provider.complete(&ChatRequest { messages: req_msgs, schema: proposal::schema(), seed: None });
            let conn = self.db.conn();
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    conn.execute("UPDATE rt_model_requests SET status='transport_error', error=?2, finished_at=?3 WHERE id=?1", params![req_id, redact_text(&e.to_string()), now_ms()])?;
                    return Ok(StepOutcome::Paused(format!("The model provider did not give a usable reply: {e}")));
                }
            };
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE rt_model_requests SET status='ok', response_redacted=?2, provider_reasoning_redacted=?3, structured_output_mode=?4, finished_at=?5 WHERE id=?1",
                params![req_id, serde_json::to_string(&json!({"content": redact_text(&resp.content)}))?, resp.provider_reasoning.as_deref().map(redact_text), resp.structured_mode_used, now_ms()],
            )?;
            ChainStore::append_with(&tx, &format!("{req_id}#response"), &json!({"rt_table": "rt_model_requests", "response_sha256": sha256_hex(resp.content.as_bytes()), "has_provider_reasoning": resp.provider_reasoning.is_some()}), now_ms())?;
            let attempt_id = new_id(ids::ATTEMPT);
            match proposal::validate(&resp.content, &ctx) {
                Ok(p) => {
                    insert(&tx, "rt_proposal_attempts", json!({"id": attempt_id, "model_request_id": req_id, "session_id": session_id, "step": step, "attempt": attempt, "raw_redacted": redact_text(&resp.content), "valid": true, "errors": [], "created_at": now_ms()}))?;
                    let dec_id = new_id(ids::DECISION);
                    let (kind, tool, args) = match &p.proposed_action {
                        Some((t, a)) => ("action", Some(t.clone()), Some(a.clone())),
                        None => ("response", None, None),
                    };
                    let approval_required = tool.as_ref().map(|t| self.cfg.limits.approval_required_tools.iter().any(|x| x == t)).unwrap_or(false);
                    let red_args = args.as_ref().map(redact_deep);
                    if let (Some(a), Some(r)) = (&args, &red_args) {
                        if a != r {
                            lock(EXACT_ARGS.get()).insert(dec_id.clone(), a.clone());
                        }
                    }
                    let mut prop = p.to_json();
                    prop["proposed_action"] = match (&tool, &red_args) {
                        (Some(t), Some(a)) => json!({"tool": t, "arguments": a}),
                        _ => Value::Null,
                    };
                    insert(&tx, "rt_decisions", json!({"id": dec_id, "session_id": session_id, "model_request_id": req_id, "snapshot_id": snapshot_id, "attempt_id": attempt_id, "step": step, "kind": kind,
                        "proposal": redact_deep(&prop), "instruction_ids": p.instruction_ids, "evidence_ids": p.evidence_ids, "tool": tool, "args": red_args, "args_sha256": args.as_ref().map(args_sha),
                        "approval_required": approval_required, "revises_decision_id": null, "created_at": now_ms()}))?;
                    tx.commit()?;
                    drop(conn);
                    checks::run_for_decision(&self.db, &dec_id)?;
                    return Ok(if kind == "action" {
                        StepOutcome::Action
                    } else if p.needs_clarification {
                        StepOutcome::Clarify(p.response_text.clone().unwrap_or_else(|| p.stated_rationale.clone()))
                    } else {
                        StepOutcome::Response
                    });
                }
                Err(errs) => {
                    insert(&tx, "rt_proposal_attempts", json!({"id": attempt_id, "model_request_id": req_id, "session_id": session_id, "step": step, "attempt": attempt, "raw_redacted": redact_text(&resp.content), "valid": false, "errors": errs, "created_at": now_ms()}))?;
                    tx.commit()?;
                    drop(conn);
                    checks::run_for_attempt(&self.db, &attempt_id)?;
                    last_errors = errs.iter().map(|e| format!("{}: {}", e.code, e.message)).collect::<Vec<_>>().join("; ");
                    repair = vec![
                        ChatMessage { role: "assistant".into(), content: resp.content.chars().take(4000).collect() },
                        ChatMessage { role: "user".into(), content: format!(
                            "The runtime rejected that proposal and nothing was executed. Problems: {last_errors}. \
                             If a file operation is still needed, set proposed_action to {{\"tool\": ..., \"arguments\": {{...}}}} using exactly these argument names: {}. \
                             If the task is finished, set proposed_action to null and put your answer in response_text. Return one corrected decision proposal JSON object.",
                            tools::TOOLS.iter().map(|t| format!("{}({})", t.name, t.args.iter().map(|(a, _)| *a).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>().join("; ")
                        ) },
                    ];
                }
            }
        }
        Ok(StepOutcome::Paused(format!(
            "No valid decision proposal after {} attempts; nothing was executed. Last problems: {last_errors}",
            self.cfg.limits.max_validation_retries + 1
        )))
    }

    fn provider_budget(&self) -> usize {
        self.cfg.provider.context_limit_chars.max(4000)
    }

    // ---- execution -------------------------------------------------------------------------------------
    /// Executes an accepted decision. `args` must be byte-identical (canonical JSON) to the arguments
    /// accepted with the decision; any change requires a new decision.
    pub fn execute_decision(&self, decision_id: &str, args: &Value) -> Result<String> {
        let (session_id, req_id, tool, sha, approval_required): (String, String, Option<String>, Option<String>, i64) = self
            .db
            .conn()
            .query_row("SELECT session_id, model_request_id, tool, args_sha256, approval_required FROM rt_decisions WHERE id=?1 AND kind='action'", [decision_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?
            .ok_or_else(|| RtError::NotFound(format!("accepted action decision {decision_id}")))?;
        let tool = tool.unwrap_or_default();
        if Some(args_sha(args)) != sha {
            return Err(RtError::Refused("Arguments differ from the accepted decision. A new decision is required.".into()));
        }
        let approval_ref = if approval_required != 0 {
            self.db.conn().query_row("SELECT id FROM rt_approvals WHERE decision_id=?1", [decision_id], |r| r.get::<_, String>(0)).optional()?.ok_or_else(|| RtError::Refused("This action requires approval.".into()))?
        } else {
            "not_required".to_string()
        };
        let (wd, protected) = session_paths(&self.db.conn(), &session_id)?;
        let root = PathBuf::from(&wd);
        let action_id = new_id(ids::ACTION);
        {
            let conn = self.db.conn();
            let used: i64 = conn.query_row("SELECT tool_calls_used FROM rt_sessions WHERE id=?1", [&session_id], |r| r.get(0))?;
            if used as u32 >= self.cfg.limits.max_tool_calls {
                return Err(RtError::Refused(format!("Tool-call limit reached ({}).", self.cfg.limits.max_tool_calls)));
            }
            let tx = conn.unchecked_transaction()?;
            // UNIQUE(decision_id) makes a second execution of the same decision impossible.
            insert(&tx, "rt_actions", json!({"id": action_id, "decision_id": decision_id, "session_id": session_id, "model_request_id": req_id, "tool": tool, "args": redact_deep(args), "args_sha256": args_sha(args), "approval_ref": approval_ref, "created_at": now_ms()}))
                .map_err(|e| match e {
                    RtError::Db(rusqlite::Error::SqliteFailure(f, _)) if f.code == rusqlite::ErrorCode::ConstraintViolation => RtError::Refused("This decision already has an action; it will not be executed twice.".into()),
                    e => e,
                })?;
            insert(&tx, "rt_action_states", json!({"action_id": action_id, "state": "prepared", "detail": null, "at": now_ms()}))?;
            tx.execute("UPDATE rt_sessions SET tool_calls_used=tool_calls_used+1, updated_at=?2 WHERE id=?1", params![session_id, now_ms()])?;
            tx.commit()?;
        }
        let before = Snapshotter::capture(&root, &[]);
        {
            let conn = self.db.conn();
            let tx = conn.unchecked_transaction()?;
            insert(&tx, "rt_action_states", json!({"action_id": action_id, "state": "started", "detail": null, "at": now_ms()}))?;
            tx.commit()?;
        }
        if self.crash_after_start {
            return Err(RtError::Invalid("simulated crash after start".into()));
        }
        let outcome = tools::dispatch(&tool, args, &root, &protected);
        let after = Snapshotter::capture(&root, &[]);
        self.record_result(&session_id, &action_id, &tool, &outcome, &before, &after)?;
        checks::run_for_action(&self.db, &action_id)?;
        Ok(action_id)
    }

    fn record_result(&self, session_id: &str, action_id: &str, tool: &str, outcome: &tools::ToolOutcome, before: &agenttracker_core::fs_observer::Manifest, after: &agenttracker_core::fs_observer::Manifest) -> Result<()> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        let now = now_ms();
        let ver_id = new_id(ids::VERIFICATION);
        let changes = diff(before, after);
        insert(&tx, "rt_verifications", json!({"id": ver_id, "action_id": action_id, "session_id": session_id, "method": "manifest_diff",
            "summary": format!("{} change(s) observed in the working folder by comparing file hashes before and after the action", changes.len()),
            "detail": {"files_before": before.files().count(), "files_after": after.files().count(), "note": "Independent of the tool's own report. Changes made by other processes in the same window would also appear."}, "created_at": now}))?;
        for (path, kind, b, a) in &changes {
            insert(&tx, "rt_changes", json!({"id": new_id(ids::CHANGE), "verification_id": ver_id, "action_id": action_id, "session_id": session_id, "rel_path": path, "change_kind": kind, "before_sha256": b, "after_sha256": a, "observed_at": now}))?;
        }
        let label = next_label(&tx, session_id, "evidence")?;
        let ev_text = if outcome.ok { serde_json::to_string(&outcome.output)? } else { format!("ERROR: {}", outcome.error.clone().unwrap_or_default()) };
        let mut ev = ContextItem::new(new_id(ids::EVIDENCE), label, "evidence", "tool_result", "tool_result", &format!("Tool-reported result of {tool} ({action_id})"), Some(action_id.into()), ev_text);
        ev.produced_by_action_id = Some(action_id.into());
        let ev_id = ev.id.clone();
        add_item(&tx, session_id, ev)?;
        let status = if outcome.ok { "succeeded" } else { "failed" };
        insert(&tx, "rt_results", json!({"id": new_id(ids::RESULT), "action_id": action_id, "status": status, "output_redacted": redact_deep(&outcome.output), "error": outcome.error.as_deref().map(redact_text), "evidence_id": ev_id, "finished_at": now}))?;
        insert(&tx, "rt_action_states", json!({"action_id": action_id, "state": status, "detail": outcome.error, "at": now}))?;
        tx.commit()?;
        Ok(())
    }

    /// After a crash: actions left `started` become `unknown` (the tool may or may not have run);
    /// actions left `prepared` were never dispatched and become `failed`. Nothing is re-executed.
    /// Where possible a reconciliation observation is recorded (e.g. copy destination exists and
    /// matches the source), but the state stays `unknown` for the user to review.
    pub fn recover_unknown(db: &Db) -> Result<Vec<String>> {
        let conn = db.conn();
        let rows = store::rows(&conn, "SELECT a.id, a.session_id, a.tool, a.args, (SELECT state FROM rt_action_states s WHERE s.action_id=a.id ORDER BY seq DESC LIMIT 1) AS state FROM rt_actions a", &[])?;
        let mut touched = Vec::new();
        let tx = conn.unchecked_transaction()?;
        for r in rows {
            let id = r["id"].as_str().unwrap_or_default().to_string();
            let sid = r["session_id"].as_str().unwrap_or_default().to_string();
            match r["state"].as_str() {
                Some("started") => {
                    insert(&tx, "rt_action_states", json!({"action_id": id, "state": "unknown", "detail": "Process stopped after the action started and before its result was recorded.", "at": now_ms()}))?;
                    if let Some(note) = reconcile(&tx, &sid, r["tool"].as_str().unwrap_or(""), &r["args"])? {
                        insert(&tx, "rt_verifications", json!({"id": new_id(ids::VERIFICATION), "action_id": id, "session_id": sid, "method": "reconciliation", "summary": note, "detail": {"state_kept": "unknown"}, "created_at": now_ms()}))?;
                    }
                    tx.execute("UPDATE rt_sessions SET status='paused', status_reason='Recovered after an interruption: an action outcome is unknown.', updated_at=?2 WHERE id=?1", params![sid, now_ms()])?;
                    touched.push(id);
                }
                Some("prepared") => {
                    insert(&tx, "rt_action_states", json!({"action_id": id, "state": "failed", "detail": "Process stopped before the action was dispatched; it did not run.", "at": now_ms()}))?;
                    touched.push(id);
                }
                _ => {}
            }
        }
        tx.commit()?;
        Ok(touched)
    }

    /// Asks the model to explain an earlier decision after the fact. The answer is stored as a NEW
    /// decision (kind `retrospective`) linked via `revises_decision_id`; the original record and its
    /// pre-action stated rationale are never modified.
    pub fn record_retrospective(&self, decision_id: &str) -> Result<String> {
        let orig = store::row(&self.db.conn(), "SELECT * FROM rt_decisions WHERE id=?1", &[&decision_id])?.ok_or_else(|| RtError::NotFound(decision_id.into()))?;
        let session_id = orig["session_id"].as_str().unwrap_or_default().to_string();
        let msgs = vec![
            ChatMessage { role: "system".into(), content: "Explain briefly, after the fact, why the earlier proposal was made. Reply as JSON {\"explanation\": string}.".into() },
            ChatMessage { role: "user".into(), content: format!("Earlier proposal: {}", orig["proposal"]) },
        ];
        let resp = self.provider.complete(&ChatRequest { messages: msgs.clone(), schema: json!({"type": "object", "properties": {"explanation": {"type": "string"}}, "required": ["explanation"]}), seed: None })?;
        let info = self.provider.info();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction()?;
        let req_id = new_id(ids::MODEL_REQUEST);
        insert(&tx, "rt_model_requests", json!({"id": req_id, "session_id": session_id, "snapshot_id": orig["snapshot_id"], "step": orig["step"], "attempt": 0, "purpose": "retrospective",
            "provider": info.provider, "endpoint": info.endpoint, "model": info.model, "model_digest": info.model_digest, "settings": self.provider.settings(), "runtime_version": RUNTIME_VERSION,
            "structured_output_mode": resp.structured_mode_used, "payload_redacted": {"messages": msgs.iter().map(|m| json!({"role": m.role, "content": redact_text(&m.content)})).collect::<Vec<_>>()},
            "response_redacted": {"content": redact_text(&resp.content)}, "status": "ok", "replay_captured": false, "started_at": now_ms(), "finished_at": now_ms()}))?;
        let att = new_id(ids::ATTEMPT);
        insert(&tx, "rt_proposal_attempts", json!({"id": att, "model_request_id": req_id, "session_id": session_id, "step": orig["step"], "attempt": 0, "raw_redacted": redact_text(&resp.content), "valid": true, "errors": [], "created_at": now_ms()}))?;
        let id = new_id(ids::DECISION);
        let text = proposal::extract_json(&resp.content).ok().and_then(|(v, _)| v["explanation"].as_str().map(String::from)).unwrap_or_else(|| resp.content.clone());
        insert(&tx, "rt_decisions", json!({"id": id, "session_id": session_id, "model_request_id": req_id, "snapshot_id": orig["snapshot_id"], "attempt_id": att, "step": orig["step"], "kind": "retrospective",
            "proposal": {"retrospective_explanation": redact_text(&text), "label": "Retrospective explanation (written after execution; not the pre-action stated rationale)"},
            "instruction_ids": [], "evidence_ids": [], "tool": null, "args": null, "args_sha256": null, "approval_required": false, "revises_decision_id": decision_id, "created_at": now_ms()}))?;
        tx.commit()?;
        Ok(id)
    }
}

fn reconcile(conn: &Connection, session_id: &str, tool: &str, args: &Value) -> Result<Option<String>> {
    let (wd, _) = session_paths(conn, session_id)?;
    let root = Path::new(&wd);
    let hash = |rel: &str| tools::resolve(root, rel).ok().and_then(|p| std::fs::read(p).ok()).map(|b| sha256_hex(&b));
    Ok(match tool {
        "copy_file" => {
            let (s, d) = (args["source"].as_str().unwrap_or(""), args["destination"].as_str().unwrap_or(""));
            match (hash(s), hash(d)) {
                (Some(a), Some(b)) if a == b => Some(format!("Destination {d} exists and matches the source. The copy probably completed, but the result was never recorded.")),
                (_, Some(_)) => Some(format!("Destination {d} exists but does not match the source.")),
                (_, None) => Some(format!("Destination {d} does not exist; the copy probably did not complete.")),
            }
        }
        "write_text_file" => {
            let p = args["path"].as_str().unwrap_or("");
            Some(if hash(p).is_some() { format!("{p} exists; its content cannot be compared because written content is not retained.") } else { format!("{p} does not exist.") })
        }
        _ => None,
    })
}

fn diff(before: &agenttracker_core::fs_observer::Manifest, after: &agenttracker_core::fs_observer::Manifest) -> Vec<(String, &'static str, Option<String>, Option<String>)> {
    let mut out = Vec::new();
    for a in after.files() {
        match before.get(&a.rel_path) {
            None => out.push((a.rel_path.clone(), "added", None, a.sha256.clone())),
            Some(b) if b.sha256 != a.sha256 => out.push((a.rel_path.clone(), "modified", b.sha256.clone(), a.sha256.clone())),
            _ => {}
        }
    }
    for b in before.files() {
        if after.get(&b.rel_path).is_none() {
            out.push((b.rel_path.clone(), "removed", b.sha256.clone(), None));
        }
    }
    out
}

pub(crate) fn session_paths(conn: &Connection, session_id: &str) -> Result<(String, Vec<String>)> {
    let (wd, prot): (String, String) = conn
        .query_row("SELECT s.working_dir, t.protected_paths FROM rt_sessions s JOIN rt_tasks t ON t.id=s.task_id WHERE s.id=?1", [session_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| RtError::NotFound(session_id.into()))?;
    Ok((wd, serde_json::from_str(&prot).unwrap_or_default()))
}

fn next_label(conn: &Connection, session_id: &str, kind: &str) -> Result<String> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM rt_context_items WHERE session_id=?1 AND kind=?2", params![session_id, kind], |r| r.get(0))?;
    Ok(format!("{kind}_{}", n + 1))
}

fn add_item(conn: &Connection, session_id: &str, it: ContextItem) -> Result<()> {
    if it.redaction_applied {
        lock(EXACT.get()).insert(it.id.clone(), it.content.clone());
    }
    insert(conn, "rt_context_items", json!({"id": it.id, "session_id": session_id, "label": it.label, "kind": it.kind, "role": it.role, "trust": it.trust, "source": it.source, "source_ref": it.source_ref,
        "is_excerpt": it.is_excerpt, "excerpt_range": it.excerpt_range, "content_redacted": it.content_redacted, "content_sha256": sha256_hex(it.content.as_bytes()), "redaction_applied": it.redaction_applied,
        "produced_by_action_id": it.produced_by_action_id, "created_at": now_ms()}))
}

/// Statuses a session may be retried from: paused, or finished (completed / failed).
pub const RETRYABLE_STATUSES: &[&str] = &["paused", "completed", "failed"];

/// Builds the task spec for "Retry with this model": the same request, protected paths and
/// documents as `session_id`, linked to it through `parent_session_id`. Nothing is written.
/// A built-in demo (title matches and its folder is under `demo_base`) gets a fresh demo folder,
/// so the retry starts from the same files the first run did. Other tasks reuse their folder as it is now.
pub fn retry_spec(db: &Db, session_id: &str, demo_base: Option<&Path>) -> Result<TaskSpec> {
    let conn = db.conn();
    let s = store::row(&conn, "SELECT s.status, t.title, t.instruction, t.working_dir, t.protected_paths FROM rt_sessions s JOIN rt_tasks t ON t.id=s.task_id WHERE s.id=?1", &[&session_id])?
        .ok_or_else(|| RtError::NotFound("this run".into()))?;
    let status = s["status"].as_str().unwrap_or("");
    if !RETRYABLE_STATUSES.contains(&status) {
        let why = match status {
            "running" => "This run is still going. Wait for it to stop, then try again.",
            "awaiting_approval" => "This run is waiting for your approval. Approve it or let it finish first.",
            _ => "This run can only be retried once it has paused or finished.",
        };
        return Err(RtError::Invalid(why.into()));
    }
    let title = s["title"].as_str().unwrap_or("").to_string();
    let working_dir = PathBuf::from(s["working_dir"].as_str().unwrap_or(""));
    let (items, _) = load_items(&conn, session_id)?;
    drop(conn);
    let parent = Some(session_id.to_string());
    if let Some(base) = demo_base {
        let in_base = match (working_dir.canonicalize(), base.canonicalize()) {
            (Ok(w), Ok(b)) => w.starts_with(b),
            _ => working_dir.starts_with(base),
        };
        for which in [crate::demo::Demo::A, crate::demo::Demo::B] {
            if in_base && title == crate::demo::title(which) {
                let (_, mut spec) = crate::demo::prepare(base, which)?;
                spec.parent_session_id = parent;
                return Ok(spec);
            }
        }
    }
    // The exact request (if still in memory), otherwise the stored, redacted copy.
    let instruction = items.iter().find(|i| i.label == "instruction_2").map(|i| i.content.clone()).unwrap_or_else(|| s["instruction"].as_str().unwrap_or("").to_string());
    let protected_paths = s["protected_paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let documents = items
        .iter()
        .filter(|i| i.kind == "evidence" && i.role == "retrieved" && i.produced_by_action_id.is_none() && i.source.starts_with("Document "))
        .filter_map(|i| {
            let path = i.source_ref.clone()?;
            Some(DocSpec { path, trust: i.trust.clone(), max_chars: i.content.chars().count().max(d_doc_chars()) })
        })
        .collect();
    Ok(TaskSpec { title, instruction, working_dir, protected_paths, documents, idempotency_key: None, parent_session_id: parent })
}

/// Loads a session's items in creation order. Returns (items, [(label,id)] whose exact content is unavailable).
pub(crate) fn load_items(conn: &Connection, session_id: &str) -> Result<(Vec<ContextItem>, Vec<(String, String)>)> {
    let rows = store::rows(conn, "SELECT * FROM rt_context_items WHERE session_id=?1 ORDER BY created_at, rowid", &[&session_id])?;
    let exact = lock(EXACT.get());
    let mut missing = Vec::new();
    let items = rows
        .into_iter()
        .map(|r| {
            let s = |k: &str| r[k].as_str().map(String::from);
            let id = s("id").unwrap_or_default();
            let red = s("content_redacted").unwrap_or_default();
            let applied = r["redaction_applied"].as_i64().unwrap_or(0) != 0;
            let content = if applied {
                exact.get(&id).cloned().unwrap_or_else(|| {
                    missing.push((s("label").unwrap_or_default(), id.clone()));
                    red.clone()
                })
            } else {
                red.clone()
            };
            ContextItem { id, label: s("label").unwrap_or_default(), kind: s("kind").unwrap_or_default(), role: s("role").unwrap_or_default(), trust: s("trust").unwrap_or_default(), source: s("source").unwrap_or_default(),
                source_ref: s("source_ref"), is_excerpt: r["is_excerpt"].as_i64().unwrap_or(0) != 0, excerpt_range: s("excerpt_range"), content, content_redacted: red, redaction_applied: applied, produced_by_action_id: s("produced_by_action_id") }
        })
        .collect();
    Ok((items, missing))
}

/// Drops the in-memory exact-content caches. Equivalent to what a process restart does; used by
/// tests to exercise the "only the redacted copy is available" paths.
pub fn clear_exact_cache() {
    lock(EXACT.get()).clear();
    lock(EXACT_ARGS.get()).clear();
}
