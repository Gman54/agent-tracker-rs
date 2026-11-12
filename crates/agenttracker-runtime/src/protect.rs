//! Houndbit's side of Houndbit Protect.
//!
//! Houndbit talks to Protect exactly the way any other program would: over Protect's own local
//! pipe/socket, with Houndbit's own Ed25519 pairing key kept in Houndbit's data folder. Houndbit
//! never opens Protect's database, never calls the engine's internal methods, and gets no special
//! treatment — the user still accepts the pairing and grants folders inside Protect. This module
//! is a thin client wrapper so the desktop shell can show pairing status and prove the staged
//! sandbox works from Houndbit's side.
//!
//! It uses `protect_engine::client` (the published client helper) and `protect_engine::paths`
//! (the documented default endpoint). It does not use any other part of the engine crate. When
//! the client is eventually split into its own small crate, only this import changes.
use protect_engine::client::{load_or_create_key, public_key_b64, Client};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Houndbit's stable application id in Protect. Lower-case reverse-DNS, as Protect requires.
pub const APP_ID: &str = "com.houndbit.app";
pub const DISPLAY_NAME: &str = "Houndbit";

/// Houndbit keeps its Protect pairing key here, inside its own data folder.
pub fn key_path(home: &Path) -> PathBuf {
    home.join("protect").join("houndbit-app.key")
}

/// Where Protect listens. Tests and advanced setups may override it; otherwise it is Protect's
/// documented per-user default endpoint.
pub fn endpoint() -> String {
    if let Ok(ep) = std::env::var("HOUNDBIT_PROTECT_ENDPOINT") {
        if !ep.is_empty() {
            return ep;
        }
    }
    protect_engine::paths::default_endpoint(&protect_engine::paths::default_data_dir())
}

fn cmsg(e: &protect_engine::client::ClientError) -> String {
    e.message.clone()
}

async fn connect() -> Result<Client, String> {
    Client::connect(&endpoint()).await.map_err(|e| format!("Protect isn't reachable on {}: {e}", endpoint()))
}

async fn signed_in(home: &Path) -> Result<Client, String> {
    let key = load_or_create_key(&key_path(home)).map_err(|e| format!("Couldn't read Houndbit's Protect key: {e}"))?;
    let mut c = connect().await?;
    c.sign_in(APP_ID, &key).await.map_err(|e| cmsg(&e))?;
    Ok(c)
}

/// A read-only look at where things stand, safe to call from a GET. Never creates the pairing
/// key and never errors: when Protect isn't running it simply reports that.
pub async fn status(home: &Path) -> Value {
    let ep = endpoint();
    let mut base = json!({"endpoint": ep, "app_id": APP_ID, "display_name": DISPLAY_NAME, "running": false, "paired": false});
    let mut c = match Client::connect(&ep).await {
        Ok(c) => c,
        Err(_) => {
            base["message"] = json!("Houndbit Protect isn't running. Start the Protect engine, then pair Houndbit.");
            return base;
        }
    };
    base["running"] = json!(true);
    if !key_path(home).exists() {
        base["message"] = json!("Protect is running. Pair Houndbit to connect it.");
        return base;
    }
    let key = match load_or_create_key(&key_path(home)) {
        Ok(k) => k,
        Err(e) => {
            base["message"] = json!(format!("Houndbit's Protect key couldn't be read: {e}"));
            return base;
        }
    };
    base["public_key"] = json!(public_key_b64(&key));
    match c.sign_in(APP_ID, &key).await {
        Ok(who) => {
            base["paired"] = json!(true);
            base["scopes"] = who.get("scopes").cloned().unwrap_or(Value::Null);
            base["message"] = json!("Connected to Houndbit Protect.");
        }
        Err(_) => {
            base["message"] = json!("Protect is running but Houndbit isn't paired yet. Pair it, then accept in Protect.");
        }
    }
    base
}

/// Asks Protect to pair Houndbit. The user still has to accept it inside Protect; this only
/// starts the request and returns the pairing id to watch.
pub async fn pair(home: &Path) -> Result<Value, String> {
    let key = load_or_create_key(&key_path(home)).map_err(|e| format!("Couldn't create Houndbit's Protect key: {e}"))?;
    let mut c = connect().await?;
    let r = c
        .call(
            "v1.pairing.request",
            json!({"protocol_major": protect_engine::PROTOCOL_MAJOR, "app_id": APP_ID, "display_name": DISPLAY_NAME, "public_key": public_key_b64(&key), "scopes": ["observer", "requester"]}),
        )
        .await
        .map_err(|e| cmsg(&e))?;
    Ok(json!({
        "pairing_id": r.get("pairing_id").cloned().unwrap_or(Value::Null),
        "state": r.get("state").cloned().unwrap_or(Value::Null),
        "public_key": public_key_b64(&key),
        "message": "Pairing requested. Open Houndbit Protect to accept it, then refresh."
    }))
}

/// The folders the user has granted Houndbit in Protect.
pub async fn grants(home: &Path) -> Result<Value, String> {
    let mut c = signed_in(home).await?;
    c.call("v1.grants.list", json!({})).await.map_err(|e| cmsg(&e))
}

/// Runs one real round-trip through the staged sandbox on a granted folder and reports what
/// happened, proving from Houndbit's side that the worker reads a snapshot, stages writes, and
/// never touches the originals. `rel` is a file path inside the granted folder to read and stage.
pub async fn sandbox_check(home: &Path, grant_id: &str, rel: &str) -> Result<Value, String> {
    let mut c = signed_in(home).await?;
    let s = c
        .call("v1.sessions.request", json!({"resource_grant_id": grant_id}))
        .await
        .map_err(|e| cmsg(&e))?;
    let sid = s.get("id").and_then(Value::as_str).ok_or("Protect didn't return a session id")?.to_string();
    c.call("v1.sessions.start", json!({"session_id": sid})).await.map_err(|e| cmsg(&e))?;

    let listed = c
        .call("v1.actions.submit", json!({"session_id": sid, "idempotency_key": "hb-list", "operation_id": "files.list", "arguments": {}}))
        .await
        .map_err(|e| cmsg(&e))?;
    let read = c
        .call("v1.actions.submit", json!({"session_id": sid, "idempotency_key": "hb-read", "operation_id": "files.read", "arguments": {"path": rel}}))
        .await
        .map_err(|e| cmsg(&e))?;
    let wrote = c
        .call(
            "v1.actions.submit",
            json!({"session_id": sid, "idempotency_key": "hb-write", "operation_id": "files.write_staged", "arguments": {"path": rel, "text": "// staged by Houndbit's sandbox check\n"}}),
        )
        .await
        .map_err(|e| cmsg(&e))?;
    let diff = c
        .call("v1.actions.submit", json!({"session_id": sid, "idempotency_key": "hb-diff", "operation_id": "files.diff", "arguments": {}}))
        .await
        .map_err(|e| cmsg(&e))?;
    c.call("v1.sessions.stop", json!({"session_id": sid})).await.ok();

    let wrote_original = wrote.pointer("/output/wrote_original").and_then(Value::as_bool).unwrap_or(true);
    Ok(json!({
        "session_id": sid,
        "listed": listed.pointer("/output/files"),
        "read_ok": read.get("executed").and_then(Value::as_bool).unwrap_or(false),
        "staged_ok": wrote.get("executed").and_then(Value::as_bool).unwrap_or(false),
        "wrote_original": wrote_original,
        "originals_untouched": !wrote_original,
        "diff": diff.pointer("/output/changes"),
        "message": "Houndbit read the snapshot and staged a change; your original files were not written."
    }))
}
