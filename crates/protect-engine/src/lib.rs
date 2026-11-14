//! Houndbit Protect engine.
//!
//! What this crate does today: app pairing (Ed25519 keys, approved only through the local admin
//! path), challenge/proof authentication, policy decisions (allow / deny / require_approval),
//! sessions, idempotent actions, single-use approvals and an ordered event log, all served over
//! a length-prefixed JSON-RPC 2.0 API on a per-user named pipe (Unix socket off Windows, for tests).
//!
//! It also runs the allowed actions in a staged file worker (see [`worker`]): for each session it
//! takes a one-time read-only snapshot of the granted folder and gives the session a separate
//! writable staging folder. The four typed file operations (list / read / write_staged / diff)
//! read the snapshot and write only into staging, so a session can never write or delete the
//! user's real files. This is a confined *file* worker for those four operations, not an OS
//! process sandbox: the profile has no shell, no code execution and no network, so there is no
//! arbitrary process to isolate. The capabilities report still says the `gate` and `isolate`
//! modes are unavailable. This crate never imports Houndbit code or opens Houndbit's database.
pub mod client;
pub mod engine;
pub mod errors;
pub mod framing;
pub mod paths;
pub mod policy;
pub mod rpc;
pub mod store;
pub mod transport;
pub mod worker;

pub use engine::Engine;
pub use errors::{Code, ProtectError};

pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_MAJOR: u64 = 1;
pub const PROTOCOL_MINOR: u64 = 0;
pub const API_DOCS_VERSION: &str = "1.0-draft";

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
