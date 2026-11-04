//! Contradiction and support checks.
//!
//! Deterministic checks run automatically and are recorded with basis `deterministic`.
//! The optional model-based analysis is recorded with basis `inferred`, carries the excerpts it
//! relied on, and can only append findings — it never touches decisions, actions or results.
use crate::ids::{self, new_id};
use crate::provider::{ChatMessage, ChatRequest, Provider};
use crate::store::{self, insert};
use crate::tools;
use crate::{now_ms, Result};
use agenttracker_core::db::Db;
use serde_json::{json, Value};

fn finding(db: &Db, session_id: &str, decision_id: Option<&str>, action_id: Option<&str>, check: &str, basis: &str, summary: String, excerpts: Value) -> Result<()> {
    let conn = db.conn();
    // One finding per (check, decision/action) for deterministic checks.
    let dup: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rt_findings WHERE check_type=?1 AND basis='deterministic' AND IFNULL(decision_id,'')=IFNULL(?2,'') AND IFNULL(action_id,'')=IFNULL(?3,'')",
        rusqlite::params![check, decision_id, action_id],
        |r| r.get(0),
    )?;
    if dup > 0 && basis == "deterministic" {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    insert(&tx, "rt_findings", json!({"id": new_id(ids::FINDING), "session_id": session_id, "decision_id": decision_id, "action_id": action_id, "check_type": check, "basis": basis, "summary": summary, "excerpts": excerpts, "created_at": now_ms()}))?;
    tx.commit()?;
    Ok(())
}

/// Checks on an accepted decision: cited IDs exist and were included in that request.
/// (Validation already enforces this; the check re-derives it from the stored records so imported
/// or tampered records are still flagged.)
pub fn run_for_decision(db: &Db, decision_id: &str) -> Result<()> {
    let Some(d) = store::row(&db.conn(), "SELECT * FROM rt_decisions WHERE id=?1", &[&decision_id])? else { return Ok(()) };
    let sid = d["session_id"].as_str().unwrap_or_default().to_string();
    let snap = d["snapshot_id"].as_str().unwrap_or_default().to_string();
    let cited: Vec<String> = d["instruction_ids"].as_array().into_iter().chain(d["evidence_ids"].as_array()).flatten().filter_map(|v| v.as_str().map(String::from)).collect();
    for id in cited {
        let (exists, included): (i64, i64) = {
            let conn = db.conn();
            let e = conn.query_row("SELECT COUNT(*) FROM rt_context_items WHERE id=?1 AND session_id=?2", [&id, &sid], |r| r.get(0))?;
            let i = conn.query_row("SELECT COUNT(*) FROM rt_snapshot_items WHERE snapshot_id=?1 AND item_id=?2 AND included=1", [&snap, &id], |r| r.get(0))?;
            (e, i)
        };
        if exists == 0 {
            finding(db, &sid, Some(decision_id), None, "cited_evidence_missing", "deterministic", format!("Cited item {id} does not exist in this session."), json!([{"cited": id}]))?;
        } else if included == 0 {
            finding(db, &sid, Some(decision_id), None, "cited_evidence_not_in_request", "deterministic", format!("Cited item {id} was not included in the request that produced this decision."), json!([{"cited": id}]))?;
        }
    }
    // Protected-path policy vs the proposed action (dispatcher enforces it independently).
    if let (Some(tool), Some(args)) = (d["tool"].as_str(), d.get("args")) {
        protected_conflict(db, &sid, Some(decision_id), None, tool, args)?;
    }
    Ok(())
}

fn protected_conflict(db: &Db, sid: &str, decision_id: Option<&str>, action_id: Option<&str>, tool: &str, args: &Value) -> Result<bool> {
    let (_, protected) = crate::runtime::session_paths(&db.conn(), sid)?;
    let Some(def) = tools::tool(tool) else { return Ok(false) };
    for w in def.writes {
        if let Some(p) = args[*w].as_str().and_then(|v| tools::normalize_rel(v).ok()).and_then(|n| tools::is_protected(&n, &protected)) {
            finding(db, sid, decision_id, action_id, "protected_path_conflict", "deterministic", format!("Proposed {tool} would modify '{}', which is under protected path '{p}'.", args[*w].as_str().unwrap_or("")), json!([{"tool": tool, "arguments": args}]))?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Checks on a rejected proposal attempt (e.g. a proposal that tried to write to a protected path).
pub fn run_for_attempt(db: &Db, attempt_id: &str) -> Result<()> {
    let Some(a) = store::row(&db.conn(), "SELECT * FROM rt_proposal_attempts WHERE id=?1", &[&attempt_id])? else { return Ok(()) };
    let sid = a["session_id"].as_str().unwrap_or_default().to_string();
    let errs = a["errors"].as_array().cloned().unwrap_or_default();
    let raw = a["raw_redacted"].as_str().unwrap_or_default();
    let parsed = crate::proposal::extract_json(raw).ok().map(|(v, _)| v);
    for e in &errs {
        let code = e["code"].as_str().unwrap_or("");
        let check = match code {
            "unknown_ref" => "cited_evidence_missing",
            "ref_not_in_request" => "cited_evidence_not_in_request",
            "protected_path" => "protected_path_conflict",
            _ => continue,
        };
        let conn = db.conn();
        let tx = conn.unchecked_transaction()?;
        insert(&tx, "rt_findings", json!({"id": new_id(ids::FINDING), "session_id": sid, "decision_id": null, "action_id": null, "check_type": check, "basis": "deterministic",
            "summary": format!("Rejected proposal ({attempt_id}): {}", e["message"].as_str().unwrap_or("")),
            "excerpts": [{"attempt_id": attempt_id, "proposed_action": parsed.as_ref().map(|p| p["proposed_action"].clone()), "stated_rationale": parsed.as_ref().map(|p| p["stated_rationale"].clone()), "cited": parsed.as_ref().map(|p| json!({"instruction_refs": p["instruction_refs"], "evidence_refs": p["evidence_refs"]}))}],
            "created_at": now_ms()}))?;
        tx.commit()?;
    }
    Ok(())
}

/// Checks on an executed action: executed ≠ approved, claimed success vs failed tool result,
/// mutating tool reported success but no change was independently observed.
pub fn run_for_action(db: &Db, action_id: &str) -> Result<()> {
    let conn = db.conn();
    let Some(a) = store::row(&conn, "SELECT a.*, d.args_sha256 AS decision_sha, d.tool AS decision_tool, d.proposal AS proposal FROM rt_actions a JOIN rt_decisions d ON d.id=a.decision_id WHERE a.id=?1", &[&action_id])? else { return Ok(()) };
    let result = store::row(&conn, "SELECT * FROM rt_results WHERE action_id=?1", &[&action_id])?;
    let changes: i64 = conn.query_row("SELECT COUNT(*) FROM rt_changes WHERE action_id=?1", [action_id], |r| r.get(0))?;
    drop(conn);
    let sid = a["session_id"].as_str().unwrap_or_default().to_string();
    let did = a["decision_id"].as_str().map(String::from);
    if a["args_sha256"] != a["decision_sha"] || a["tool"] != a["decision_tool"] {
        finding(db, &sid, did.as_deref(), Some(action_id), "executed_differs_from_approved", "deterministic", "The executed tool or arguments differ from the accepted decision.".into(), json!([{"executed": {"tool": a["tool"], "args": a["args"]}, "accepted_tool": a["decision_tool"]}]))?;
    }
    protected_conflict(db, &sid, did.as_deref(), Some(action_id), a["tool"].as_str().unwrap_or(""), &a["args"])?;
    if let Some(r) = result {
        let expected = a["proposal"]["expected_effect"].as_str().unwrap_or("").to_string();
        if r["status"] == "failed" {
            finding(db, &sid, did.as_deref(), Some(action_id), "claimed_success_but_failed", "deterministic",
                format!("The proposal expected: \"{expected}\". The tool reported failure: {}", r["error"].as_str().unwrap_or("")), json!([{"expected_effect": expected, "tool_error": r["error"]}]))?;
        } else if tools::tool(a["tool"].as_str().unwrap_or("")).map(|t| !t.writes.is_empty()).unwrap_or(false) && changes == 0 {
            finding(db, &sid, did.as_deref(), Some(action_id), "reported_success_not_observed", "deterministic",
                "The tool reported success, but no file change was independently observed.".into(), json!([{"tool_output": r["output_redacted"]}]))?;
        }
    }
    Ok(())
}

/// Optional model-based analysis of instruction vs rationale vs action. Recorded as `inferred` with
/// the excerpts it was given. Writes only to rt_findings.
pub fn analyze_with_model(db: &Db, provider: &dyn Provider, decision_id: &str) -> Result<String> {
    let conn = db.conn();
    let d = store::row(&conn, "SELECT * FROM rt_decisions WHERE id=?1", &[&decision_id])?.ok_or_else(|| crate::RtError::NotFound(decision_id.into()))?;
    let sid = d["session_id"].as_str().unwrap_or_default().to_string();
    let instr = store::rows(&conn, "SELECT label, content_redacted FROM rt_context_items WHERE session_id=?1 AND kind='instruction' ORDER BY created_at", &[&sid])?;
    drop(conn);
    let excerpts = json!({"instructions": instr, "stated_rationale": d["proposal"]["stated_rationale"], "proposed_action": d["proposal"]["proposed_action"]});
    let resp = provider.complete(&ChatRequest {
        messages: vec![
            ChatMessage { role: "system".into(), content: "You review records. Say whether the proposed action contradicts the instructions or the stated rationale. Reply JSON {\"contradiction\": boolean, \"summary\": string}.".into() },
            ChatMessage { role: "user".into(), content: excerpts.to_string() },
        ],
        schema: json!({"type": "object", "properties": {"contradiction": {"type": "boolean"}, "summary": {"type": "string"}}, "required": ["contradiction", "summary"]}),
        seed: None,
    })?;
    let parsed = crate::proposal::extract_json(&resp.content).ok().map(|(v, _)| v).unwrap_or(Value::Null);
    let summary = format!(
        "Inferred explanation (model-based analysis by {}): {}",
        provider.info().model,
        parsed["summary"].as_str().unwrap_or(&resp.content).chars().take(800).collect::<String>()
    );
    let id = new_id(ids::FINDING);
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    insert(&tx, "rt_findings", json!({"id": id, "session_id": sid, "decision_id": decision_id, "action_id": null, "check_type": if parsed["contradiction"] == true { "inferred_contradiction" } else { "inferred_no_contradiction" }, "basis": "inferred", "summary": summary, "excerpts": excerpts, "created_at": now_ms()}))?;
    tx.commit()?;
    Ok(id)
}
