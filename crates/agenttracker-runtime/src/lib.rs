//! Houndbit instrumented decision runtime.
//!
//! A model running through this runtime must submit a structured *decision proposal* before any
//! tool runs. The runtime records the application-level context it sent, validates the proposal,
//! assigns authoritative IDs, persists the accepted decision, then executes the action through a
//! confined dispatcher and links the tool-reported result and independently observed changes.
//!
//! What the records are: the application-level request, the model's *stated rationale* and the
//! observed execution. What they are not: a transcript of internal computation, provider-internal
//! prompts or tokenisation, or proof of which input caused an action.
//!
//! Public interface (see docs/runtime/INTERFACE.md):
//! - [`runtime::Runtime`] — start/run/approve/recover sessions.
//! - [`provider`] — `Provider` trait, `OpenAiCompatProvider` (LM Studio / Ollama / other local
//!   OpenAI-compatible servers) and `ScriptedProvider` (recorded fixtures / simulated tests).
//! - [`tools`] — the four confined tools and the dispatcher.
//! - [`why`] — the "Why this action?" bundle for one action.
//! - [`checks`] — deterministic contradiction checks (+ optional, labelled inferred analysis).
//! - [`investigate`] — experimental "Test this influence" replays (decision stage only).
//! - [`replay_vault`] — optional encrypted replay-capture store.
//! - [`relay`] — the chat recorder: a local pass-through between a chat app and LM Studio / Ollama.
//! - [`http`] — axum routes mounted under `/api/ui/runtime/*` by the desktop shell.
pub mod checks;
pub mod config;
pub mod context;
pub mod demo;
pub mod http;
pub mod ids;
pub mod investigate;
pub mod proposal;
pub mod provider;
pub mod relay;
pub mod replay_vault;
pub mod runtime;
pub mod store;
pub mod tools;
pub mod why;

/// Version recorded on every model request.
pub const RUNTIME_VERSION: &str = concat!("agenttracker-runtime/", env!("CARGO_PKG_VERSION"), "+schema.v004");
/// Version of the decision-proposal schema the model is asked to follow.
pub const PROPOSAL_SCHEMA_VERSION: &str = "decision_proposal.v1";

#[derive(Debug, thiserror::Error)]
pub enum RtError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("core: {0}")]
    Core(#[from] agenttracker_core::CoreError),
    #[error("chain: {0}")]
    Chain(#[from] tracker_common::hash_chain::ChainError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider: {0}")]
    Provider(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("refused: {0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, RtError>;

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

pub(crate) fn now_ms() -> i64 {
    tracker_common::now_ms()
}
