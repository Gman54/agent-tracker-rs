//! Experimental "Test this influence": replay the decision-generation request with the captured
//! baseline context and with one context item removed or replaced, several times each, and compare
//! the structured proposed actions with a predefined rule. No tool is ever executed here — this
//! module has no access to the dispatcher.
//!
//! Findings describe behavioural sensitivity under the tested conditions. They do not prove what
//! caused the original decision.
use crate::context::{self, ContextItem, Fidelity};
use crate::ids::{self, new_id};
use crate::proposal::{self, normalize_action, ValidationCtx};
use crate::provider::{ChatMessage, ChatRequest, Provider};
use crate::replay_vault::ReplayVault;
use crate::store::{self, insert};
use crate::{now_ms, Result, RtError};
use agenttracker_core::db::Db;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Modification {
    Remove,
    Replace { text: String },
}

impl Modification {
    fn describe(&self, label: &str) -> String {
        match self {
            Modification::Remove => format!("{label} removed from the request"),
            Modification::Replace { text } => format!("{label} replaced with {} characters of user-supplied text", text.chars().count()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvestigationSpec {
    pub decision_id: String,
    pub context_item_id: String,
    pub modification: Modification,
    #[serde(default = "d_trials")]
    pub trials_per_arm: u32,
    #[serde(default)]
    pub seed_base: Option<i64>,
}

fn d_trials() -> u32 {
    5
}

pub const COMPARISON_METHOD: &str = "A trial matches when its valid proposal has the same tool and the same normalised arguments \
(paths normalised to forward slashes, '.' segments removed) as the original accepted decision. Invalid proposals, text responses and \
clarification requests count as non-matching and are tallied separately.";

fn describe_action(a: &Value) -> String {
    let args = a["arguments"].as_object().map(|m| m.iter().filter(|(k, _)| k.as_str() != "content").map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    format!("{}({args})", a["tool"].as_str().unwrap_or("?"))
}

/// Creates and runs an investigation synchronously. Returns the investigation id.
pub fn run(db: &Db, provider: &dyn Provider, vault: Option<&ReplayVault>, spec: &InvestigationSpec) -> Result<String> {
    let trials = spec.trials_per_arm.clamp(1, 25);
    let conn = db.conn();
    let d = store::row(&conn, "SELECT * FROM rt_decisions WHERE id=?1", &[&spec.decision_id])?.ok_or_else(|| RtError::NotFound(spec.decision_id.clone()))?;
    if d["kind"] != "action" {
        return Err(RtError::Invalid("Only action decisions can be investigated".into()));
    }
    let req = store::row(&conn, "SELECT * FROM rt_model_requests WHERE id=?1", &[&d["model_request_id"].as_str().unwrap_or("")])?.ok_or_else(|| RtError::NotFound("model request".into()))?;
    let action_id: Option<String> = store::row(&conn, "SELECT id FROM rt_actions WHERE decision_id=?1", &[&spec.decision_id])?.and_then(|r| r["id"].as_str().map(String::from));
    let sid = d["session_id"].as_str().unwrap_or_default().to_string();
    let snap = d["snapshot_id"].as_str().unwrap_or_default().to_string();
    let step = d["step"].as_i64().unwrap_or(1) as u32;
    let (all_items, _) = crate::runtime::load_items(&conn, &sid)?;
    let inc_rows = store::rows(&conn, "SELECT item_id FROM rt_snapshot_items WHERE snapshot_id=?1 AND included=1 ORDER BY position", &[&snap])?;
    let (wd, protected) = crate::runtime::session_paths(&conn, &sid)?;
    let original_action = normalize_action(d["tool"].as_str().unwrap_or(""), &d["args"]);
    drop(conn);

    let inc_ids: Vec<String> = inc_rows.iter().filter_map(|r| r["item_id"].as_str().map(String::from)).collect();
    let mut included: Vec<ContextItem> = all_items.iter().filter(|i| inc_ids.contains(&i.id)).cloned().collect();
    let target = included.iter().find(|i| i.id == spec.context_item_id).cloned();
    let inv_id = new_id(ids::INVESTIGATION);
    let mut limitations: Vec<String> = vec![
        "Only the decision-generation stage is replayed; no tools are executed and later steps are not simulated.".into(),
        "Seeds are passed when the provider accepts them, but a seed does not guarantee reproducible output (batching, hardware and server versions can change results).".into(),
        "Results describe behavioural sensitivity for this model and these settings only; they do not prove what caused the original decision.".into(),
        "Provider-side chat templates, hidden prompts and tokenisation are outside the captured payload.".into(),
        format!("Small sample: {trials} trial(s) per arm."),
    ];

    let base_record = |status: &str, fidelity: &str, summary: Option<String>, limitations: &[String]| {
        json!({"id": inv_id, "action_id": action_id, "decision_id": spec.decision_id, "model_request_id": req["id"], "snapshot_id": snap, "context_item_id": spec.context_item_id,
            "modification": spec.modification, "settings": {"provider": provider.info(), "settings": provider.settings(), "recorded_model": req["model"], "recorded_digest": req["model_digest"], "recorded_settings": req["settings"]},
            "trials_per_arm": trials, "comparison_method": COMPARISON_METHOD, "status": status, "replay_fidelity": fidelity, "confounded": false, "confound_detail": null,
            "summary": summary, "counts": null, "limitations": limitations, "created_at": now_ms(), "finished_at": now_ms()})
    };
    let not_replayable = |why: String, mut lim: Vec<String>| -> Result<String> {
        lim.insert(0, why.clone());
        let conn = db.conn();
        let tx = conn.unchecked_transaction()?;
        insert(&tx, "rt_investigations", base_record("not_replayable", "unavailable", Some(format!("A faithful replay cannot be performed: {why}")), &lim))?;
        tx.commit()?;
        Ok(inv_id.clone())
    };

    let Some(target) = target else {
        return not_replayable("the selected item was not included in the request that produced this decision, so removing it cannot change that request.".into(), limitations);
    };
    let info = provider.info();
    if req["model"].as_str() != Some(info.model.as_str()) {
        return not_replayable(format!("the recorded model was '{}' but the configured model is '{}'.", req["model"].as_str().unwrap_or("?"), info.model), limitations);
    }
    match (req["model_digest"].as_str(), info.model_digest.as_deref()) {
        (Some(a), Some(b)) if a != b => return not_replayable(format!("the model digest changed ({a} recorded, {b} now)."), limitations),
        (None, _) | (_, None) => limitations.push("The model digest is unavailable, so the same model version cannot be confirmed.".into()),
        _ => {}
    }

    // Best available content: encrypted replay vault > in-memory exact > redacted audit copy.
    let vaulted = match vault {
        Some(v) => v.load(&db.conn(), req["id"].as_str().unwrap_or(""))?,
        None => None,
    };
    if let Some(vp) = &vaulted {
        for it in included.iter_mut() {
            if let Some(c) = vp["items"].as_array().and_then(|a| a.iter().find(|x| x["id"] == it.id.as_str())).and_then(|x| x["content"].as_str()) {
                it.content = c.to_string();
            }
        }
    }
    let extra: Vec<ChatMessage> = match &vaulted {
        Some(vp) => serde_json::from_value::<Vec<ChatMessage>>(vp["messages"].clone()).unwrap_or_default().into_iter().skip(4).collect(),
        None => serde_json::from_value::<Vec<ChatMessage>>(req["payload_redacted"]["messages"].clone()).unwrap_or_default().into_iter().skip(4).collect(),
    };
    let refs: Vec<&ContextItem> = included.iter().collect();
    let mut baseline = context::render(&refs, step, Fidelity::Exact);
    baseline.extend(extra.iter().cloned());
    let recorded_sha = req["payload_redacted"]["exact_messages_sha256"].as_str().unwrap_or("");
    let fidelity = if context::messages_sha(&baseline) == recorded_sha { "exact_application_payload" } else { "modified_redacted" };
    if fidelity != "exact_application_payload" {
        limitations.push("MODIFIED REPLAY: the exact original payload is not available (redacted content and no replay capture), so the baseline differs from what the model originally received.".into());
    }

    let modified_items: Vec<ContextItem> = included
        .iter()
        .filter_map(|i| {
            if i.id != target.id {
                return Some(i.clone());
            }
            match &spec.modification {
                Modification::Remove => None,
                Modification::Replace { text } => {
                    let mut c = i.clone();
                    c.content = text.clone();
                    c.content_redacted = tracker_common::redaction::redact_text(text);
                    Some(c)
                }
            }
        })
        .collect();
    let mrefs: Vec<&ContextItem> = modified_items.iter().collect();
    let mut modified = context::render(&mrefs, step, Fidelity::Exact);
    modified.extend(extra.iter().cloned());

    // Confounds: other included content derived from the selected item.
    let mut confounds = Vec::new();
    {
        let conn = db.conn();
        for it in &included {
            if it.id == target.id {
                continue;
            }
            if let Some(aid) = &it.produced_by_action_id {
                let cited: Option<String> = conn.query_row("SELECT d.evidence_ids || d.instruction_ids FROM rt_actions a JOIN rt_decisions d ON d.id=a.decision_id WHERE a.id=?1", [aid], |r| r.get(0)).ok();
                if cited.map(|c| c.contains(&target.id)).unwrap_or(false) {
                    confounds.push(format!("{} is the result of an action whose decision cited {}", it.label, target.label));
                }
            }
            if shares_text(&target.content, &it.content) {
                confounds.push(format!("{} contains text that also appears in {}", it.label, target.label));
            }
        }
        if extra.iter().any(|m| shares_text(&target.content, &m.content)) {
            confounds.push("A repair message in the request repeats text from the selected item".into());
        }
    }
    let confounded = !confounds.is_empty();
    if confounded {
        limitations.push("CONFOUNDED: information derived from the selected item remains in the modified request; this is not a clean isolation.".into());
    }

    let make_ctx = |items: &[ContextItem]| ValidationCtx {
        included: items.iter().map(|i| (i.label.clone(), (i.id.clone(), i.kind.clone()))).collect(),
        known: all_items.iter().map(|i| i.label.clone()).collect(),
        root: Path::new(&wd),
        protected: &protected,
        tool_calls_remaining: u32::MAX,
    };
    let base_ctx = make_ctx(&included);
    let mod_ctx = make_ctx(&modified_items);

    {
        let conn = db.conn();
        let tx = conn.unchecked_transaction()?;
        let mut rec = base_record("running", fidelity, None, &limitations);
        rec["confounded"] = json!(confounded);
        rec["confound_detail"] = json!(confounds.join("; "));
        rec["finished_at"] = Value::Null;
        insert(&tx, "rt_investigations", rec)?;
        tx.commit()?;
    }

    let mut counts = json!({});
    for (arm, msgs, ctx) in [("baseline", &baseline, &base_ctx), ("modified", &modified, &mod_ctx)] {
        let (mut matches, mut valid, mut errors) = (0u32, 0u32, 0u32);
        let mut by_outcome: BTreeMap<String, u32> = BTreeMap::new();
        for t in 0..trials {
            let seed = spec.seed_base.map(|s| s + t as i64);
            let (proposal_v, norm, ok, err) = match provider.complete(&ChatRequest { messages: msgs.clone(), schema: proposal::schema(), seed }) {
                Err(e) => {
                    errors += 1;
                    (Value::Null, json!("provider_error"), false, Some(e.to_string()))
                }
                Ok(r) => match proposal::validate(&r.content, ctx) {
                    Ok(p) => {
                        valid += 1;
                        let n = match &p.proposed_action {
                            Some((tool, args)) => normalize_action(tool, args),
                            None if p.needs_clarification => json!("clarification"),
                            None => json!("text_response"),
                        };
                        (p.to_json(), n, true, None)
                    }
                    Err(es) => (json!({"raw": tracker_common::redaction::redact_text(&r.content)}), json!("invalid"), false, Some(serde_json::to_string(&es)?)),
                },
            };
            if norm == original_action {
                matches += 1;
            }
            let key = if norm.is_string() { norm.as_str().unwrap_or("").to_string() } else { describe_action(&norm) };
            *by_outcome.entry(key).or_default() += 1;
            let conn = db.conn();
            let tx = conn.unchecked_transaction()?;
            insert(&tx, "rt_replay_trials", json!({"id": new_id(ids::TRIAL), "investigation_id": inv_id, "arm": arm, "trial_no": t + 1, "seed": seed, "proposal": tracker_common::redaction::redact_deep(&proposal_v), "normalized_action": norm, "valid": ok, "error": err, "created_at": now_ms()}))?;
            tx.commit()?;
        }
        counts[arm] = json!({"matches_original": matches, "trials": trials, "valid": valid, "provider_errors": errors, "by_outcome": by_outcome});
    }
    let desc = describe_action(&original_action);
    let summary = format!(
        "Baseline proposed {desc} in {}/{trials} trials. Modified context ({}) proposed it in {}/{trials} trials. This is evidence of behavioural sensitivity under the tested conditions, not proof of the original cause.{}",
        counts["baseline"]["matches_original"], spec.modification.describe(&target.label), counts["modified"]["matches_original"],
        if confounded { " Confounded: derived information remained in the modified context." } else { "" }
    );
    let conn = db.conn();
    conn.execute(
        "UPDATE rt_investigations SET status='completed', summary=?2, counts=?3, limitations=?4, finished_at=?5 WHERE id=?1",
        rusqlite::params![inv_id, summary, counts.to_string(), serde_json::to_string(&limitations)?, now_ms()],
    )?;
    tracker_common::hash_chain::ChainStore::append_with(&conn, &format!("{inv_id}#completed"), &json!({"rt_table": "rt_investigations", "summary": summary, "counts": counts}), now_ms())?;
    Ok(inv_id)
}

/// True when `b` contains a 60-character window of `a` (or all of `a` when it is shorter).
fn shares_text(a: &str, b: &str) -> bool {
    let a = a.trim();
    if a.len() < 24 {
        return false;
    }
    let chars: Vec<char> = a.chars().collect();
    if chars.len() <= 60 {
        return b.contains(a);
    }
    let mut i = 0;
    while i + 60 <= chars.len() {
        let w: String = chars[i..i + 60].iter().collect();
        if b.contains(&w) {
            return true;
        }
        i += 30;
    }
    false
}
