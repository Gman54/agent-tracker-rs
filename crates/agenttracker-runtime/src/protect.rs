//! Houndbit's side of Houndbit Protect.
//!
//! Houndbit talks to Protect the way any other program would: over Protect's own local
//! pipe/socket, signing in with Houndbit's own Ed25519 key kept in Houndbit's data folder.
//!
//! To keep it to one install, Houndbit also starts the Protect engine itself when it opens (see
//! `autostart`), unless a separate Protect engine is already running for this user. Houndbit links
//! itself to the engine it hosts automatically (`ensure_linked`); there is no "connect" step. While Houndbit
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
    ensure_linked(home).await?;
    let key = load_or_create_key(&key_path(home)).map_err(|e| format!("Couldn't read Houndbit's Protect key: {e}"))?;
    let mut c = connect().await?;
    c.sign_in(APP_ID, &key).await.map_err(|e| cmsg(&e))?;
    Ok(c)
}

/// Where things stand. Never errors: when Protect isn't running it simply reports that.
///
/// Houndbit links itself to Protect on its own: when Houndbit hosts the engine (the normal case)
/// the pairing is requested and accepted right here, with no button and no question, because it
/// is the owner's own app talking to its own built-in engine. When a separate Protect program is
/// running instead, Houndbit sends it one pairing request and the owner accepts it there, as with
/// any other app.
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
    let hosted = is_hosted();
    if hosted {
        if let Err(e) = ensure_linked(home).await {
            base["message"] = json!(format!("Houndbit couldn't link itself to Protect: {e}"));
            return base;
        }
    } else if !key_path(home).exists() {
        base["message"] = json!(ask_separate_protect_once(home).await);
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
            base["message"] = json!("Houndbit is linked to Protect automatically.");
        }
        Err(_) if hosted => base["message"] = json!("Houndbit is still linking itself to Protect. Check again in a moment."),
        Err(_) => base["message"] = json!(ask_separate_protect_once(home).await),
    }
    base
}

static LINKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static ASKED_SEPARATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Links Houndbit to the Protect engine it hosts, if it isn't already: requests the pairing and
/// accepts it straight away. Does nothing when a separate Protect program is running (that one
/// asks its owner, see `status`). Safe to call often; it only signs in when already linked.
pub async fn ensure_linked(home: &Path) -> Result<(), String> {
    if !is_hosted() {
        return Ok(());
    }
    let _one_at_a_time = LINKING.lock().await;
    let key = load_or_create_key(&key_path(home)).map_err(|e| format!("Couldn't create Houndbit's Protect key: {e}"))?;
    if connect().await?.sign_in(APP_ID, &key).await.is_ok() {
        return Ok(());
    }
    let r = match pair(home).await {
        Ok(r) => r,
        // Protect knows Houndbit by an older key (for example Houndbit's data folder was reset).
        // It is Houndbit's own entry, so replace it.
        Err(e) if e.contains("already paired") => {
            admin_remove_app(APP_ID)?;
            pair(home).await?
        }
        Err(e) => return Err(e),
    };
    let id = r["pairing_id"].as_str().ok_or("Protect didn't return a pairing id")?.to_string();
    admin_pairing(&id, true)?;
    Ok(())
}

/// With a separate Protect program, sends it one pairing request per run of Houndbit.
async fn ask_separate_protect_once(home: &Path) -> String {
    if !ASKED_SEPARATE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        if let Err(e) = pair(home).await {
            ASKED_SEPARATE.store(false, std::sync::atomic::Ordering::SeqCst);
            return format!("A separate Protect program is running, and Houndbit couldn't ask it to link up: {e}");
        }
    }
    "A separate Protect program is running. Houndbit has asked it to link up; accept the request in that program, then check again.".to_string()
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

/// Houndbit signed in to Protect with its own key, for the model helper's actions.
pub async fn signed_in_client(home: &Path) -> Result<Client, String> {
    signed_in(home).await
}

/// A session's staged changes with old and new text, for you to look over.
pub fn admin_changes(session_id: &str) -> Result<Value, String> {
    hosted()?.admin_session_changes(session_id).map_err(emsg)
}

/// Writes the chosen staged changes (None = all) into the real folder. Only your click calls this.
pub fn admin_apply(session_id: &str, paths: Option<&[String]>) -> Result<Value, String> {
    hosted()?.admin_apply_changes(session_id, paths).map_err(emsg)
}

/// Throws away the chosen staged changes (None = all).
pub fn admin_discard(session_id: &str, paths: Option<&[String]>) -> Result<Value, String> {
    hosted()?.admin_discard_changes(session_id, paths).map_err(emsg)
}

// ---- Folder guard for AI helpers (Claude Code, Cursor, the chat recorder) ----
// The list is a small file the hook program reads by itself (see tracker_common::guard), so
// guarding keeps working when the Houndbit window is closed.

use tracker_common::guard;

const ASK_SHOW_MS: i64 = 100_000;

pub fn guard_overview(home: &Path) -> Value {
    let list = guard::load(home);
    let cursor_state = match agenttracker_core::connectors::cursor::CursorConnector::for_user().guard_ready() {
        None => "not_connected",
        Some(false) => "needs_update",
        Some(true) => "ready",
    };
    let folders: Vec<Value> = list.folders.iter().map(|f| json!({"path": guard::display_path(&f.path), "mode": f.mode})).collect();
    let claude = agenttracker_core::connectors::claude_code::ClaudeConnector::for_user();
    let claude_state = match claude.guard_ready() {
        None => "not_connected",
        Some(false) => "needs_update",
        Some(true) => "ready",
    };
    json!({
        "folders": folders,
        "asks": guard::pending_asks(home, ASK_SHOW_MS),
        "log": guard::recent_log(home, 30),
        "claude": claude_state,
        "cursor": cursor_state,
        "addon_enabled": guard::addon_active(&list),
    })
}

/// The Protect add-on switch (on by default while Protect is being tested). Off means the folder
/// guard lets everything through; the hook reads the same `enabled` flag from guard.json.
/// FUTURE LICENSE CHECK: when Protect becomes a paid add-on, refuse to switch it on here without
/// a valid license (and check it in `tracker_common::guard::addon_active` as well).
pub fn set_addon(home: &Path, enabled: bool) -> Result<Value, String> {
    let mut list = guard::load(home);
    list.enabled = enabled;
    guard::save(home, &list).map_err(|e| format!("Couldn't save the setting: {e}"))?;
    Ok(guard_overview(home))
}

pub fn guard_set_folder(home: &Path, folder: &str, mode: &str) -> Result<Value, String> {
    let folder = folder.trim().trim_matches('"');
    if folder.is_empty() {
        return Err("Type the folder's full path first, for example F:\\Test.".into());
    }
    if mode != "block" && mode != "ask" {
        return Err("Pick \"Always block\" or \"Ask me first\".".into());
    }
    let p = Path::new(folder);
    if !p.is_absolute() {
        return Err("Use the folder's full path, starting with the drive letter, for example F:\\Test.".into());
    }
    if !p.is_dir() {
        return Err(format!("Couldn't find a folder at {folder}. Check the spelling."));
    }
    let shown = std::fs::canonicalize(p).map(|c| guard::display_path(&c.to_string_lossy())).unwrap_or_else(|_| folder.to_string());
    let mut list = guard::load(home);
    let key = guard::norm(&shown);
    match list.folders.iter_mut().find(|f| guard::norm(&f.path) == key) {
        Some(f) => f.mode = mode.to_string(),
        None => list.folders.push(guard::GuardFolder { path: shown, mode: mode.to_string() }),
    }
    guard::save(home, &list).map_err(|e| format!("Couldn't save the list: {e}"))?;
    Ok(guard_overview(home))
}

pub fn guard_remove_folder(home: &Path, folder: &str) -> Result<Value, String> {
    let mut list = guard::load(home);
    let key = guard::norm(folder);
    let before = list.folders.len();
    list.folders.retain(|f| guard::norm(&f.path) != key);
    if list.folders.len() == before {
        return Err("That folder isn't on the list.".into());
    }
    guard::save(home, &list).map_err(|e| format!("Couldn't save the list: {e}"))?;
    Ok(guard_overview(home))
}

pub fn guard_answer(home: &Path, id: &str, allow: bool) -> Result<Value, String> {
    if !guard::answer(home, id, allow) {
        return Err("That request is no longer waiting (it may have timed out and been stopped).".into());
    }
    Ok(guard_overview(home))
}
