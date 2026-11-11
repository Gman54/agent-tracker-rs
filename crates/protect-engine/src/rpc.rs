//! JSON-RPC 2.0 dispatch with per-connection authentication. The caller's identity comes from
//! the proof it gave on this connection, never from a field in the request.
use crate::engine::Engine;
use crate::errors::{Code, ProtectError, Result};
use crate::{framing, now_ms, policy, API_DOCS_VERSION, ENGINE_VERSION, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use serde_json::{json, Value};
use std::sync::Arc;

const PRE_AUTH: &[&str] = &["v1.pairing.request", "v1.pairing.status", "v1.auth.challenge", "v1.auth.prove"];
const NONCE_MS: i64 = 60_000;
/// Per-connection limit on requests per minute.
pub const RATE_PER_MIN: usize = 600;

#[derive(Default)]
pub struct ConnState {
    app: Option<(String, Vec<String>)>,
    challenge: Option<(String, String, i64)>,
    recent: std::collections::VecDeque<i64>,
}

fn p_str<'a>(p: &'a Value, k: &str) -> Result<&'a str> {
    p.get(k).and_then(Value::as_str).ok_or_else(|| ProtectError::new(Code::InvalidArguments, format!("missing text parameter \"{k}\"")))
}

fn check_major(p: &Value) -> Result<()> {
    match p.get("protocol_major").and_then(Value::as_u64) {
        Some(PROTOCOL_MAJOR) => Ok(()),
        Some(m) => Err(ProtectError::new(Code::UnsupportedVersion, format!("This engine speaks protocol {PROTOCOL_MAJOR}.x; the app asked for {m}.x."))),
        None => Err(ProtectError::new(Code::UnsupportedVersion, "protocol_major is required")),
    }
}

pub fn capabilities() -> Value {
    json!({
        "modes": {"observe": "available", "gate": "unavailable", "isolate": "unavailable"},
        "operations": policy::OPERATIONS.iter().map(|o| json!({"operation_id": o, "decision": "available", "execution": "unavailable"})).collect::<Vec<_>>(),
        "backends": {"windows_isolation": "not_implemented", "mcp_gateway": "not_implemented", "claude_code_gate": "not_implemented"},
        "entitlement": "not_implemented",
        "limitations": [
            "Milestone A: the engine decides and records actions but runs nothing.",
            "No OS isolation, no worker, no MCP gateway and no host hooks yet.",
            "Pairing and approvals are accepted through the local admin command, which any program running as you can also call."
        ]
    })
}

pub fn hello(engine: &Engine) -> Value {
    json!({
        "engine_id": engine.engine_id, "engine_version": ENGINE_VERSION,
        "protocol_version": format!("{PROTOCOL_MAJOR}.{PROTOCOL_MINOR}"),
        "supported_features": ["pairing", "sessions", "actions", "approvals", "events"],
        "backend_support": {"windows": "decisions_only", "macos": "unsupported", "linux": "decisions_only_for_tests"},
        "effective_limits": {"max_frame_bytes": framing::MAX_FRAME, "max_events_page": 200, "requests_per_minute": RATE_PER_MIN},
        "connector_registry_revision": 0, "api_docs_version": API_DOCS_VERSION
    })
}

fn need(scopes: &[String], s: &str) -> Result<()> {
    if scopes.iter().any(|x| x == s) {
        Ok(())
    } else {
        Err(ProtectError::new(Code::ScopeDenied, format!("This app wasn't given the \"{s}\" scope.")))
    }
}

pub fn dispatch(engine: &Engine, st: &mut ConnState, method: &str, p: &Value) -> Result<Value> {
    let now = now_ms();
    st.recent.retain(|t| now - *t < 60_000);
    if st.recent.len() >= RATE_PER_MIN {
        return Err(ProtectError::new(Code::RateLimited, "Too many requests on this connection; slow down."));
    }
    st.recent.push_back(now);
    if PRE_AUTH.contains(&method) {
        check_major(p)?;
        return match method {
            "v1.pairing.request" => {
                let scopes: Vec<String> = p.get("scopes").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
                engine.pairing_request(p_str(p, "app_id")?, p_str(p, "display_name")?, p_str(p, "public_key")?, &scopes)
            }
            "v1.pairing.status" => engine.pairing_status(p_str(p, "pairing_id")?),
            "v1.auth.challenge" => {
                let app = p_str(p, "app_id")?.to_string();
                let nonce = hex::encode(rand::random::<[u8; 32]>());
                st.challenge = Some((app, nonce.clone(), now + NONCE_MS));
                Ok(json!({"engine_id": engine.engine_id, "nonce": nonce, "expires_in_ms": NONCE_MS}))
            }
            _ => {
                // The nonce is single-use: taken out before checking, whatever the outcome.
                let (app, nonce, exp) = st.challenge.take().ok_or_else(|| ProtectError::new(Code::AuthFailed, "Ask for a challenge first."))?;
                if p_str(p, "app_id")? != app || exp < now {
                    return Err(ProtectError::new(Code::AuthFailed, "The challenge expired or was for a different app."));
                }
                let scopes = engine.verify_proof(&app, &nonce, p_str(p, "signature")?)?;
                st.app = Some((app.clone(), scopes.clone()));
                Ok(json!({"authenticated": true, "app_id": app, "scopes": scopes}))
            }
        };
    }
    let (app, scopes) = st.app.clone().ok_or_else(|| ProtectError::new(Code::AuthRequired, "Sign in with v1.auth.challenge and v1.auth.prove first."))?;
    match method {
        "v1.hello" => {
            check_major(p)?;
            Ok(hello(engine))
        }
        "v1.health" => Ok(json!({"engine": "running", "records": "ok", "execution": "unavailable"})),
        "v1.capabilities.get" => Ok(capabilities()),
        "v1.sessions.request" => {
            need(&scopes, "requester")?;
            engine.sessions_request(&app, p.get("profile").and_then(Value::as_str).unwrap_or(policy::DEFAULT_PROFILE), p_str(p, "resource_grant_id")?)
        }
        "v1.sessions.get" => {
            need_any(&scopes)?;
            engine.sessions_get(&app, p_str(p, "session_id")?)
        }
        "v1.sessions.list" => {
            need_any(&scopes)?;
            Ok(json!({"sessions": engine.sessions_list(&app)?}))
        }
        "v1.sessions.start" | "v1.sessions.pause" | "v1.sessions.stop" => {
            need(&scopes, "requester")?;
            let to = match method {
                "v1.sessions.start" => "running",
                "v1.sessions.pause" => "paused",
                _ => "completed",
            };
            engine.sessions_transition(&app, p_str(p, "session_id")?, to)
        }
        "v1.actions.submit" => {
            need(&scopes, "requester")?;
            let args = p.get("arguments").cloned().unwrap_or(json!({}));
            engine.actions_submit(&app, p_str(p, "session_id")?, p_str(p, "idempotency_key")?, p_str(p, "operation_id")?, &args)
        }
        "v1.actions.get" => {
            need_any(&scopes)?;
            engine.actions_get(&app, p_str(p, "action_id")?)
        }
        "v1.approvals.list" => {
            need(&scopes, "approver")?;
            Ok(json!({"approvals": engine.approvals_list()?}))
        }
        "v1.approvals.resolve" => {
            need(&scopes, "approver")?;
            let approve = match p_str(p, "decision")? {
                "approve" => true,
                "deny" => false,
                _ => return Err(ProtectError::new(Code::InvalidArguments, "decision must be \"approve\" or \"deny\"")),
            };
            engine.approvals_resolve(p_str(p, "approval_id")?, p_str(p, "nonce")?, approve)
        }
        "v1.events.read" => {
            need_any(&scopes)?;
            let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            engine.events_read(&app, p_str(p, "session_id")?, p.get("cursor").and_then(Value::as_str), limit)
        }
        _ => Err(ProtectError::new(Code::UnsupportedOperation, format!("Unknown method \"{method}\""))),
    }
}

fn need_any(scopes: &[String]) -> Result<()> {
    if scopes.iter().any(|s| s == "observer" || s == "requester") {
        Ok(())
    } else {
        Err(ProtectError::new(Code::ScopeDenied, "This app can't read sessions."))
    }
}

pub fn error_json(id: Value, rpc_code: i64, e: &ProtectError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": rpc_code, "message": e.message, "data": {"protect_code": e.code}}})
}

/// Handles one raw frame and returns the response frame.
pub fn handle_frame(engine: &Arc<Engine>, st: &mut ConnState, body: &[u8]) -> Value {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return error_json(Value::Null, -32700, &ProtectError::new(Code::InvalidArguments, format!("Not valid JSON: {e}"))),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return error_json(id, -32600, &ProtectError::new(Code::InvalidArguments, "jsonrpc must be \"2.0\""));
    }
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return error_json(id, -32600, &ProtectError::new(Code::InvalidArguments, "method is required"));
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));
    match dispatch(engine, st, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => error_json(id, if e.message.starts_with("Unknown method") { -32601 } else { -32000 }, &e),
    }
}
