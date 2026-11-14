//! Stable error codes. Clients branch on `Code`, never on the English message.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Code {
    AuthRequired,
    AuthFailed,
    ScopeDenied,
    LicenseRequired,
    UnsupportedVersion,
    UnsupportedOperation,
    InvalidArguments,
    EngineUnavailable,
    PolicyDenied,
    ApprovalRequired,
    StaleResource,
    RateLimited,
    AuditUnavailable,
    OutcomeUnknown,
    NotFound,
    IdempotencyConflict,
    InvalidState,
    FrameTooLarge,
}

impl Code {
    pub fn as_str(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ProtectError {
    pub code: Code,
    pub message: String,
}

impl ProtectError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl From<rusqlite::Error> for ProtectError {
    fn from(e: rusqlite::Error) -> Self {
        // A record that can't be written must stop the action (fail closed), never let it through.
        Self::new(Code::AuditUnavailable, format!("The engine couldn't read or write its records: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, ProtectError>;
