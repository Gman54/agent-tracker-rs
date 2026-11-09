//! Houndbit core: observes AI coding agents and independently verifies their file work.
//! Observational only — nothing here blocks or enforces agent behaviour.
pub mod collector;
pub mod conversation;
pub mod connectors;
pub mod db;
pub mod demo;
pub mod engine;
pub mod event;
pub mod fs_observer;
pub mod gitwork;
pub mod queue;
pub mod report;
pub mod server;
pub mod store;
pub mod task;
pub mod verifiers;

pub use tracker_common::now_ms;

pub const APP_NAME: &str = "Houndbit";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const EVENT_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hash chain error: {0}")]
    Chain(#[from] tracker_common::hash_chain::ChainError),
    #[error("invalid lifecycle transition from {from} to {to}")]
    Lifecycle { from: String, to: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Per-user Houndbit directory (`%USERPROFILE%\.agenttracker` / `~/.agenttracker`).
/// `AGENTTRACKER_HOME` overrides it (used by tests and portable setups).
pub fn agenttracker_home() -> std::path::PathBuf {
    if let Some(h) = std::env::var_os("AGENTTRACKER_HOME").filter(|v| !v.is_empty()) {
        return h.into();
    }
    user_home().join(".agenttracker")
}

pub fn user_home() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
        .map(Into::into)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}
