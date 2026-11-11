//! Houndbit Protect engine — Milestone A.
//!
//! What this crate does today: app pairing (Ed25519 keys, approved only through the local admin
//! path), challenge/proof authentication, policy decisions (allow / deny / require_approval),
//! sessions, idempotent actions, single-use approvals and an ordered event log, all served over
//! a length-prefixed JSON-RPC 2.0 API on a per-user named pipe (Unix socket off Windows, for tests).
//!
//! What it does NOT do yet: run anything. There is no worker and no OS isolation, so every
//! authorized action ends as `failed` with `ENGINE_UNAVAILABLE` and "nothing was run". The
//! capabilities report says gate and isolate are unavailable. This crate never imports Houndbit
//! code or opens Houndbit's database.
pub mod client;
pub mod engine;
pub mod errors;
pub mod framing;
pub mod paths;
pub mod policy;
pub mod rpc;
pub mod store;
pub mod transport;

pub use engine::Engine;
pub use errors::{Code, ProtectError};

pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_MAJOR: u64 = 1;
pub const PROTOCOL_MINOR: u64 = 0;
pub const API_DOCS_VERSION: &str = "1.0-draft";

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
