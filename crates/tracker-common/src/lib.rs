//! Shared building blocks used by AgentTracker.
pub mod canonical;
pub mod hash_chain;
pub mod license;
pub mod machine_id;
pub mod redaction;
pub mod security;

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
