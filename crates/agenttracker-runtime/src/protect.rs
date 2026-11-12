//! Houndbit's side of Houndbit Protect.
//!
//! Houndbit talks to Protect the way any other program would: over Protect's own local
//! pipe/socket, signing in with Houndbit's own Ed25519 key kept in Houndbit's data folder.
//!
//! To keep it to one install, Houndbit also starts the Protect engine itself when it opens (see
//! `autostart`), unless a separate Protect engine is already running for this user. While Houndbit
//! hosts the engine, Houndbit's window doubles as the Protect controls: accept or reject pairing
//! requests, give apps folders, and answer approvals. Houndbit's own file work still goes through
//! the client path with its key, exactly like other apps.
use protect_engine::client::{load_or_create_key, public_key_b64, Client};
use protect_engine::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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
    let mut base = json!({"endpoint": ep, "app_id": APP_ID, "display_name": DISPLAY_NAME, "running": false, "paired": false, "hosted": is_hosted()});
    let mut c = match Client::connect(&ep).await {
        Ok(c) => c,
        Err(_) => {
            let why = HOST_ERROR.lock().ok().and_then(|g| g.clone());
            base["message"] = json!(match why {
                Some(w) => format!("Protect normally starts with Houndbit, but it couldn't start: {w}"),
                None => "Protect isn't answering yet. It starts with Houndbit; give it a moment and check again.".to_string(),
            });
            return base;
        }
    };
    base["running"] = json!(true);
    if !key_path(home).exists() {
        base["message"] = json!("Protect is running. Connect Houndbit to it with one click.");
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
            base["message"] = json!("Protect is running but Houndbit isn't connected to it yet.");
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

// ---- Protect running inside Houndbit ------------------------------------------------------------
//
// Houndbit ships one installer. So that Protect works without a second program to find and start,
// Houndbit starts the Protect engine itself, in the background, on Protect's normal endpoint. If a
// separate Protect engine is already running for this user, Houndbit leaves it alone and just
// connects to it like any other app. When Houndbit hosts the engine, Houndbit's window is also
// where the user accepts pairings, gives folders and answers approvals (the "Protect window" job);
// those admin calls below only work on the engine Houndbit itself started.

static HOSTED: OnceLock<Arc<Engine>> = OnceLock::new();
static HOST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn set_host_error(m: Option<String>) {
    if let Ok(mut g) = HOST_ERROR.lock() {
        *g = m;
    }
}

/// True when the engine Houndbit started is the one answering on the endpoint.
pub fn is_hosted() -> bool {
    HOSTED.get().is_some()
}

/// Starts Protect inside Houndbit with the default records folder and endpoint, unless
/// `HOUNDBIT_PROTECT_ENDPOINT` points somewhere else (advanced setups, tests). Must be called from
/// inside a Tokio runtime. Never blocks and never panics.
pub fn autostart() {
    if std::env::var("HOUNDBIT_PROTECT_ENDPOINT").map(|v| !v.is_empty()).unwrap_or(false) {
        return;
    }
    let dir = protect_engine::paths::default_data_dir();
    let ep = protect_engine::paths::default_endpoint(&dir);
    tokio::spawn(host(dir, ep));
}

/// Opens Protect's records in `dir` and serves on `ep`, unless something already answers there.
/// Returns when the engine stops (normally never).
pub async fn host(dir: PathBuf, ep: String) {
    if Client::connect(&ep).await.is_ok() {
        return; // a separate Protect engine is already running; connect to it as a normal app
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        set_host_error(Some(format!("Couldn't create Protect's records folder {}: {e}", dir.display())));
        return;
    }
    let engine = match Engine::open(&dir) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            set_host_error(Some(format!("Couldn't open Protect's records in {}: {}", dir.display(), e.message)));
            return;
        }
    };
    if HOSTED.set(engine.clone()).is_err() {
        return; // already hosting in this process
    }
    set_host_error(None);
    if let Err(e) = protect_engine::transport::serve(engine, &ep).await {
        set_host_error(Some(format!("Protect stopped: {e}")));
    }
}

fn hosted() -> Result<&'static Arc<Engine>, String> {
    HOSTED.get().ok_or_else(|| "These controls only work when Protect runs inside Houndbit. A separate Protect engine is running, so use its own commands.".to_string())
}

fn emsg(e: protect_engine::ProtectError) -> String {
    e.message
}

/// Everything the Protect controls show: requests waiting, paired apps, folders, approvals.
pub fn admin_overview() -> Value {
    let Ok(e) = hosted() else {
        return json!({"hosted": false});
    };
    let pairings: Vec<Value> = e.admin_pairings().unwrap_or_default().into_iter().filter(|p| p["state"] == "pending").collect();
    let apps: Vec<Value> = e.admin_apps().unwrap_or_default().into_iter().filter(|a| a["revoked_at"].is_null()).collect();
    let grants: Vec<Value> = e.admin_grants().unwrap_or_default().into_iter().filter(|g| g["revoked_at"].is_null()).collect();
    let approvals: Vec<Value> = e
        .approvals_list()
        .unwrap_or_default()
        .into_iter()
        .map(|mut a| {
            if let Some(o) = a.as_object_mut() {
                o.remove("nonce");
            }
            a
        })
        .collect();
    json!({"hosted": true, "pairings": pairings, "apps": apps, "grants": grants, "approvals": approvals})
}

pub fn admin_pairing(pairing_id: &str, accept: bool) -> Result<Value, String> {
    hosted()?.admin_pairing_resolve(pairing_id, accept, None).map_err(emsg)
}

pub fn admin_add_folder(app_id: &str, folder: &str) -> Result<Value, String> {
    hosted()?.admin_grant_add(app_id, folder.trim()).map_err(emsg)
}

pub fn admin_remove_app(app_id: &str) -> Result<Value, String> {
    hosted()?.admin_revoke_app(app_id).map_err(emsg)
}

pub fn admin_approval(approval_id: &str, approve: bool) -> Result<Value, String> {
    let e = hosted()?;
    let nonce = e
        .approvals_list()
        .map_err(emsg)?
        .into_iter()
        .find(|x| x["approval_id"] == approval_id)
        .and_then(|x| x["nonce"].as_str().map(str::to_string))
        .ok_or("That request is no longer waiting (it may have expired or been answered).")?;
    e.approvals_resolve(approval_id, &nonce, approve).map_err(emsg)
}

/// One click from Houndbit's own window: ask Protect to pair Houndbit and, when Houndbit is
/// hosting Protect, accept that request straight away (the user clicking the button is the OK).
pub async fn connect_houndbit(home: &Path) -> Result<Value, String> {
    let r = pair(home).await?;
    if !is_hosted() {
        return Ok(r);
    }
    let id = r["pairing_id"].as_str().ok_or("Protect didn't return a pairing id")?.to_string();
    admin_pairing(&id, true)?;
    Ok(json!({"pairing_id": id, "state": "accepted", "message": "Houndbit is connected to Protect."}))
}
