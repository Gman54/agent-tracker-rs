//! "Why this action?" bundle: every record linked to one action, grouped by what it is, with the
//! explicit provenance labels used by the UI. Cited evidence is labelled "Cited by model", never
//! as a proven cause.
use crate::store;
use crate::{Result, RtError};
use agenttracker_core::db::Db;
use serde_json::{json, Value};

pub const LABEL_AVAILABLE: &str = "Available to model";
pub const LABEL_CITED: &str = "Cited by model";
pub const LABEL_RATIONALE: &str = "Stated rationale";
pub const LABEL_TOOL_RESULT: &str = "Tool-reported result";
pub const LABEL_OBSERVED: &str = "Independently observed";
pub const LABEL_INFERRED: &str = "Inferred explanation";
pub const LABEL_UNAVAILABLE: &str = "Unavailable";

pub fn bundle_for_action(db: &Db, action_id: &str) -> Result<Value> {
    let a = store::row(&db.conn(), "SELECT * FROM rt_actions WHERE id=?1", &[&action_id])?.ok_or_else(|| RtError::NotFound(action_id.into()))?;
    let did = a["decision_id"].as_str().unwrap_or_default().to_string();
    bundle(db, &did, Some(a))
}

pub fn bundle_for_decision(db: &Db, decision_id: &str) -> Result<Value> {
    let a = store::row(&db.conn(), "SELECT * FROM rt_actions WHERE decision_id=?1", &[&decision_id])?;
    bundle(db, decision_id, a)
}

fn bundle(db: &Db, decision_id: &str, action: Option<Value>) -> Result<Value> {
    let conn = db.conn();
    let d = store::row(&conn, "SELECT * FROM rt_decisions WHERE id=?1", &[&decision_id])?.ok_or_else(|| RtError::NotFound(decision_id.into()))?;
    let sid = d["session_id"].as_str().unwrap_or_default().to_string();
    let snap = d["snapshot_id"].as_str().unwrap_or_default().to_string();
    let session = store::row(&conn, "SELECT s.*, t.title, t.instruction, t.protected_paths FROM rt_sessions s JOIN rt_tasks t ON t.id=s.task_id WHERE s.id=?1", &[&sid])?;
    let req = store::row(&conn, "SELECT * FROM rt_model_requests WHERE id=?1", &[&d["model_request_id"].as_str().unwrap_or("")])?;
    let snapshot = store::row(&conn, "SELECT * FROM rt_context_snapshots WHERE id=?1", &[&snap])?;
    let items = store::rows(&conn, "SELECT i.*, si.included, si.position FROM rt_snapshot_items si JOIN rt_context_items i ON i.id=si.item_id WHERE si.snapshot_id=?1 ORDER BY si.position", &[&snap])?;
    let attempts = store::rows(&conn, "SELECT * FROM rt_proposal_attempts WHERE session_id=?1 AND step=?2 ORDER BY created_at", &[&sid, &d["step"].as_i64().unwrap_or(0)])?;
    let (states, result, changes, verifications) = match &action {
        Some(a) => {
            let aid = a["id"].as_str().unwrap_or_default();
            (
                store::rows(&conn, "SELECT state, detail, at FROM rt_action_states WHERE action_id=?1 ORDER BY seq", &[&aid])?,
                store::row(&conn, "SELECT * FROM rt_results WHERE action_id=?1", &[&aid])?,
                store::rows(&conn, "SELECT * FROM rt_changes WHERE action_id=?1 ORDER BY rel_path", &[&aid])?,
                store::rows(&conn, "SELECT * FROM rt_verifications WHERE action_id=?1 ORDER BY created_at", &[&aid])?,
            )
        }
        None => (vec![], None, vec![], vec![]),
    };
    let approval = store::row(&conn, "SELECT * FROM rt_approvals WHERE decision_id=?1", &[&decision_id])?;
    let findings = store::rows(&conn, "SELECT * FROM rt_findings WHERE decision_id=?1 OR (action_id IS NOT NULL AND action_id=?2) OR (session_id=?3 AND decision_id IS NULL AND action_id IS NULL) ORDER BY created_at", &[&decision_id, &action.as_ref().and_then(|a| a["id"].as_str()).unwrap_or(""), &sid])?;
    let later = store::rows(&conn, "SELECT id, kind, step, proposal, revises_decision_id, created_at FROM rt_decisions WHERE session_id=?1 AND (revises_decision_id=?2 OR step>?3) ORDER BY created_at", &[&sid, &decision_id, &d["step"].as_i64().unwrap_or(0)])?;
    let investigations = store::rows(&conn, "SELECT id, status, replay_fidelity, confounded, summary, created_at FROM rt_investigations WHERE decision_id=?1 ORDER BY created_at DESC", &[&decision_id])?;
    drop(conn);

    let cited: Vec<String> = d["instruction_ids"].as_array().into_iter().chain(d["evidence_ids"].as_array()).flatten().filter_map(|v| v.as_str().map(String::from)).collect();
    let items: Vec<Value> = items
        .into_iter()
        .map(|mut i| {
            let id = i["id"].as_str().unwrap_or_default().to_string();
            let included = i["included"].as_i64().unwrap_or(0) == 1;
            let mut labels = vec![if included { LABEL_AVAILABLE } else { LABEL_UNAVAILABLE }];
            if cited.contains(&id) {
                labels.push(LABEL_CITED);
            }
            i["labels"] = json!(labels);
            i
        })
        .collect();

    let mut gaps: Vec<String> = Vec::new();
    if let Some(s) = &snapshot {
        for o in s["omitted"].as_array().cloned().unwrap_or_default() {
            gaps.push(o["label"].as_str().map(|l| format!("{l}: {}", o["reason"].as_str().unwrap_or(""))).unwrap_or_else(|| o["reason"].as_str().unwrap_or("").to_string()));
        }
    }
    if items.iter().any(|i| i["redaction_applied"].as_i64() == Some(1)) {
        gaps.push("Some context was redacted in this audit record; the redacted text is shown, not the exact text the model received.".into());
    }
    if req.as_ref().map(|r| r["replay_captured"].as_i64() != Some(1)).unwrap_or(true) {
        gaps.push("Replay capture was off for this request; only the redacted audit copy of the payload is stored.".into());
    }
    if req.as_ref().and_then(|r| r["model_digest"].as_str()).is_none() {
        gaps.push("Model digest unavailable from the provider; the exact model build cannot be confirmed.".into());
    }
    if req.as_ref().and_then(|r| r["provider_reasoning_redacted"].as_str()).is_none() {
        gaps.push("No provider-exposed reasoning was returned. Internal model computation is never visible to Houndbit.".into());
    }
    if action.is_some() && states.last().map(|s| s["state"] == "unknown").unwrap_or(false) {
        gaps.push("The execution outcome is UNKNOWN (the process stopped before the result was recorded). It was not repeated.".into());
    }
    let is_fixture = session.as_ref().map(|s| s["provider_config"]["is_fixture"] == true).unwrap_or(false);

    Ok(json!({
        "origin": "agenttracker_runtime",
        "is_fixture": is_fixture,
        "notice": "These records show what the application sent to the model, the model's stated rationale before execution, and what happened. They are not a record of the model's internal reasoning and do not prove which input caused the action.",
        "labels": {"available": LABEL_AVAILABLE, "cited": LABEL_CITED, "rationale": LABEL_RATIONALE, "tool_result": LABEL_TOOL_RESULT, "observed": LABEL_OBSERVED, "inferred": LABEL_INFERRED, "unavailable": LABEL_UNAVAILABLE},
        "session": session,
        "original_request": session.as_ref().map(|s| s["instruction"].clone()),
        "model_request": req,
        "snapshot": snapshot,
        "context_items": items,
        "decision": d,
        "proposal_attempts": attempts,
        "approval": approval,
        "action": action,
        "action_states": states,
        "result": result,
        "verifications": verifications,
        "observed_changes": changes,
        "findings": findings,
        "later_decisions": later,
        "investigations": investigations,
        "capture_gaps": gaps,
    }))
}
