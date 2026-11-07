//! Decision-proposal schema and validation.
//!
//! Reference validation (does `evidence_3` exist, and was it included in *this* request?) is kept
//! separate from semantic support: a valid reference proves the item was available to the model,
//! not that it supports the stated rationale.
use crate::tools;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const MAX_RAW_BYTES: usize = 32 * 1024;
pub const MAX_RATIONALE: usize = 1200;
pub const MAX_ITEM: usize = 400;
pub const MAX_LIST: usize = 12;

/// JSON schema sent as `response_format` (where the server supports it) and described in the prompt.
pub fn schema() -> Value {
    let str_list = json!({"type": "array", "items": {"type": "string"}});
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "instruction_refs": str_list,
            "evidence_refs": str_list,
            "stated_rationale": {"type": "string"},
            "assumptions": str_list,
            "proposed_action": {
                "anyOf": [
                    {"type": "null"},
                    {"type": "object", "additionalProperties": false,
                     "properties": {"tool": {"type": "string", "enum": tools::TOOLS.iter().map(|t| t.name).collect::<Vec<_>>()},
                                    "arguments": {"type": "object"}},
                     "required": ["tool", "arguments"]}
                ]
            },
            "expected_effect": {"type": "string"},
            "uncertainty": str_list,
            "alternatives_considered": str_list,
            "needs_clarification": {"type": "boolean"},
            "response_text": {"type": "string"}
        },
        "required": ["instruction_refs", "evidence_refs", "stated_rationale", "assumptions", "proposed_action", "expected_effect", "uncertainty", "needs_clarification"]
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VError {
    pub code: String,
    pub message: String,
}

fn ve(code: &str, message: impl Into<String>) -> VError {
    VError { code: code.into(), message: message.into() }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedProposal {
    pub instruction_refs: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub instruction_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub stated_rationale: String,
    pub assumptions: Vec<String>,
    pub proposed_action: Option<(String, Value)>,
    pub expected_effect: String,
    pub uncertainty: Vec<String>,
    pub alternatives_considered: Vec<String>,
    pub needs_clarification: bool,
    pub response_text: Option<String>,
    /// Non-fatal notes, e.g. ignored model-supplied id fields.
    pub notes: Vec<String>,
}

impl ValidatedProposal {
    pub fn to_json(&self) -> Value {
        json!({
            "schema": crate::PROPOSAL_SCHEMA_VERSION,
            "instruction_refs": self.instruction_refs, "evidence_refs": self.evidence_refs,
            "stated_rationale": self.stated_rationale, "assumptions": self.assumptions,
            "proposed_action": self.proposed_action.as_ref().map(|(t, a)| json!({"tool": t, "arguments": a})),
            "expected_effect": self.expected_effect, "uncertainty": self.uncertainty,
            "alternatives_considered": self.alternatives_considered,
            "needs_clarification": self.needs_clarification, "response_text": self.response_text,
            "notes": self.notes,
        })
    }
}

/// What the validator knows about the request that produced the proposal.
pub struct ValidationCtx<'a> {
    /// label -> (item id, kind) for items INCLUDED in this request.
    pub included: HashMap<String, (String, String)>,
    /// All labels known to the session (included or not).
    pub known: HashSet<String>,
    pub root: &'a Path,
    pub protected: &'a [String],
    pub tool_calls_remaining: u32,
}

/// Extracts the JSON object from a model reply (tolerates code fences / surrounding prose, noted).
pub fn extract_json(raw: &str) -> Result<(Value, Option<String>), VError> {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok((v, None));
    }
    let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) else {
        return Err(ve("not_json", "reply is not a JSON object"));
    };
    if b <= a {
        return Err(ve("not_json", "reply is not a JSON object"));
    }
    serde_json::from_str::<Value>(&t[a..=b]).map(|v| (v, Some("JSON object extracted from surrounding text".into()))).map_err(|e| ve("not_json", format!("invalid JSON: {e}")))
}

fn str_list(v: &Value, field: &str, errs: &mut Vec<VError>, required: bool) -> Vec<String> {
    match v.get(field) {
        None | Some(Value::Null) => {
            if required {
                errs.push(ve("schema", format!("missing field '{field}'")));
            }
            vec![]
        }
        Some(Value::Array(a)) => {
            if a.len() > MAX_LIST {
                errs.push(ve("field_too_long", format!("'{field}' has more than {MAX_LIST} entries")));
            }
            a.iter()
                .filter_map(|x| match x.as_str() {
                    Some(s) if s.len() <= MAX_ITEM => Some(s.trim().to_string()),
                    Some(_) => {
                        errs.push(ve("field_too_long", format!("an entry in '{field}' exceeds {MAX_ITEM} characters")));
                        None
                    }
                    None => {
                        errs.push(ve("schema", format!("'{field}' must contain strings")));
                        None
                    }
                })
                .collect()
        }
        Some(_) => {
            errs.push(ve("schema", format!("'{field}' must be an array of strings")));
            vec![]
        }
    }
}

pub fn validate(raw: &str, ctx: &ValidationCtx) -> Result<ValidatedProposal, Vec<VError>> {
    if raw.len() > MAX_RAW_BYTES {
        return Err(vec![ve("field_too_long", format!("reply exceeds {MAX_RAW_BYTES} bytes"))]);
    }
    let (v, extract_note) = extract_json(raw).map_err(|e| vec![e])?;
    if !v.is_object() {
        return Err(vec![ve("schema", "reply must be a JSON object")]);
    }
    let mut errs = Vec::new();
    let mut notes: Vec<String> = extract_note.into_iter().collect();
    for k in v.as_object().unwrap().keys() {
        let known = ["instruction_refs", "evidence_refs", "stated_rationale", "assumptions", "proposed_action", "expected_effect", "uncertainty", "alternatives_considered", "needs_clarification", "response_text"];
        if !known.contains(&k.as_str()) {
            // The model may not assign authoritative IDs; any such fields are ignored, never stored as IDs.
            notes.push(format!("ignored unexpected field '{k}'"));
        }
    }
    let instruction_refs = str_list(&v, "instruction_refs", &mut errs, true);
    let evidence_refs = str_list(&v, "evidence_refs", &mut errs, true);
    let assumptions = str_list(&v, "assumptions", &mut errs, true);
    let uncertainty = str_list(&v, "uncertainty", &mut errs, true);
    let alternatives_considered = str_list(&v, "alternatives_considered", &mut errs, false);
    let stated_rationale = match v.get("stated_rationale").and_then(|x| x.as_str()) {
        Some(s) if s.trim().is_empty() => {
            errs.push(ve("schema", "'stated_rationale' is empty"));
            String::new()
        }
        Some(s) if s.len() > MAX_RATIONALE => {
            errs.push(ve("field_too_long", format!("'stated_rationale' exceeds {MAX_RATIONALE} characters")));
            String::new()
        }
        Some(s) => s.trim().to_string(),
        None => {
            errs.push(ve("schema", "missing string field 'stated_rationale'"));
            String::new()
        }
    };
    let expected_effect = match v.get("expected_effect") {
        Some(Value::String(s)) if s.len() <= MAX_RATIONALE => s.trim().to_string(),
        Some(Value::String(_)) => {
            errs.push(ve("field_too_long", "'expected_effect' too long"));
            String::new()
        }
        _ => {
            errs.push(ve("schema", "missing string field 'expected_effect'"));
            String::new()
        }
    };
    let needs_clarification = match v.get("needs_clarification") {
        Some(Value::Bool(b)) => *b,
        _ => {
            errs.push(ve("schema", "missing boolean field 'needs_clarification'"));
            false
        }
    };
    let response_text = v.get("response_text").and_then(|x| x.as_str()).map(|s| s.chars().take(4000).collect::<String>());

    // References: existence in the session vs inclusion in this request are distinct failures.
    let mut resolve_refs = |refs: &[String], kind: &str| -> Vec<String> {
        let mut ids = Vec::new();
        for r in refs {
            match ctx.included.get(r) {
                Some((id, k)) if k == kind => ids.push(id.clone()),
                Some((_, k)) => errs.push(ve("wrong_ref_kind", format!("'{r}' is an {k}, not an {kind}"))),
                None if ctx.known.contains(r) => errs.push(ve("ref_not_in_request", format!("'{r}' exists but was not included in this request"))),
                None => errs.push(ve("unknown_ref", format!("'{r}' does not exist"))),
            }
        }
        ids
    };
    let instruction_ids = resolve_refs(&instruction_refs, "instruction");
    let evidence_ids = resolve_refs(&evidence_refs, "evidence");

    let proposed_action = match v.get("proposed_action") {
        None => {
            errs.push(ve("schema", "missing field 'proposed_action' (use null for no action)"));
            None
        }
        Some(Value::Null) => None,
        Some(a) => {
            let name = a.get("tool").and_then(|t| t.as_str()).unwrap_or("");
            let args = a.get("arguments").cloned().unwrap_or(Value::Null);
            match tools::tool(name) {
                None => {
                    errs.push(ve("unknown_tool", format!("tool '{name}' is not available")));
                    None
                }
                Some(def) => {
                    for (code, m) in tools::check_args(def, &args, ctx.root, ctx.protected) {
                        errs.push(ve(&code, m));
                    }
                    if ctx.tool_calls_remaining == 0 {
                        errs.push(ve("limit_exceeded", "tool-call limit for this session reached"));
                    }
                    Some((name.to_string(), args))
                }
            }
        }
    };
    if proposed_action.is_some() && needs_clarification {
        errs.push(ve("schema", "a proposal that needs clarification must not propose an action"));
    }
    if proposed_action.is_none() && !needs_clarification && response_text.as_deref().map(str::trim).unwrap_or("").is_empty() {
        errs.push(ve("schema", "with no proposed_action, provide 'response_text' or set needs_clarification"));
    }
    if instruction_refs.is_empty() && proposed_action.is_some() {
        errs.push(ve("schema", "an action proposal must cite at least one instruction reference"));
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(ValidatedProposal { instruction_refs, evidence_refs, instruction_ids, evidence_ids, stated_rationale, assumptions, proposed_action, expected_effect, uncertainty, alternatives_considered, needs_clarification, response_text, notes })
}

/// Normalised (tool, arguments) used to compare actions (args sha + investigations).
pub fn normalize_action(tool: &str, args: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(def) = tools::tool(tool) {
        for (a, is_path) in def.args {
            let raw = args.get(*a).and_then(|v| v.as_str()).unwrap_or("");
            let v = if *is_path { tools::normalize_rel(raw).unwrap_or_else(|_| raw.to_string()) } else { raw.to_string() };
            out.insert(a.to_string(), Value::String(v));
        }
    }
    json!({"tool": tool, "arguments": out})
}
