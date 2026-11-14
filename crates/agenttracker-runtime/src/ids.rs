//! Runtime-assigned identifiers. The model never assigns authoritative IDs.
//!
//! Format: `<prefix>_<uuidv7 simple>` — UUIDv7 is time-ordered with 74 random bits, so IDs are
//! collision-resistant and sort roughly by creation time. Time order is NOT a causal claim.
//!
//! Separately, context items receive short *reference labels* (`instruction_1`, `evidence_4`) that
//! are stable within a session and are the only handles the model may cite.

pub const TASK: &str = "task";
pub const SESSION: &str = "ses";
pub const SNAPSHOT: &str = "snap";
pub const MODEL_REQUEST: &str = "req";
pub const INSTRUCTION: &str = "ins";
pub const EVIDENCE: &str = "evd";
pub const DECISION: &str = "dec";
pub const ACTION: &str = "act";
pub const RESULT: &str = "res";
pub const CHANGE: &str = "chg";
pub const VERIFICATION: &str = "ver";
pub const INVESTIGATION: &str = "inv";
pub const ATTEMPT: &str = "att";
pub const APPROVAL: &str = "apr";
pub const FINDING: &str = "fnd";
pub const TRIAL: &str = "trl";

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}

pub fn has_prefix(id: &str, prefix: &str) -> bool {
    id.len() == prefix.len() + 33 && id.starts_with(prefix) && id.as_bytes()[prefix.len()] == b'_'
}
