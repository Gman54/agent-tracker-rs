//! Loopback-only HTTP server: the UI API (one-time launch token → HttpOnly session cookie + CSRF
//! header) and the hook ingest endpoint (HMAC-signed).
use crate::collector::Collector;
use crate::connectors::{claude_code::ClaudeConnector, copilot::CopilotConnector, cursor::CursorConnector, manual::ManualConnector, DetectResult};
use crate::engine::Engine;
use crate::task::{NewCheck, NewTask};
use crate::{CoreError, APP_NAME, VERSION};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tracker_common::{license, security};

pub const COOKIE: &str = "at_session";
pub const CSRF_HEADER: &str = "x-requested-with";
pub const CSRF_VALUE: &str = "agenttracker";
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

pub struct AppState {
    pub engine: Arc<Engine>,
    pub collector: Arc<Collector>,
    pub port: u16,
    pub hook_secret: String,
    pub hook_binary: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub agenttracker_home: PathBuf,
    launch_token: Mutex<Option<String>>,
    ui_sessions: Mutex<HashSet<String>>,
    /// Native folder picker supplied by the desktop shell (None in headless mode).
    pub folder_picker: OnceLock<FolderPicker>,
}

pub type FolderPicker = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Settings the UI may change, with their defaults.
pub const SETTING_DEFAULTS: &[(&str, &str)] = &[
    ("wizard_done", "0"),
    ("work_mode", ""),
    ("retention_days", "0"),
    ("redaction_patterns", ""),
    ("start_with_windows", "0"),
    ("keep_in_tray", "1"),
    ("auto_verify", "0"),
    ("export_dir", ""),
];

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Arc<Engine>,
        collector: Arc<Collector>,
        port: u16,
        launch_token: Option<String>,
        hook_secret: String,
        hook_binary: Option<PathBuf>,
        data_dir: PathBuf,
        agenttracker_home: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            collector,
            port,
            hook_secret,
            hook_binary,
            data_dir,
            agenttracker_home,
            launch_token: Mutex::new(launch_token),
            ui_sessions: Mutex::new(HashSet::new()),
            folder_picker: OnceLock::new(),
        })
    }

    /// Issues a fresh one-time launch token (e.g. when the tray re-opens the window).
    pub fn new_launch_token(&self) -> String {
        let t = security::token_urlsafe(24);
        *self.launch_token.lock().unwrap_or_else(|p| p.into_inner()) = Some(t.clone());
        t
    }

    fn has_session(&self, headers: &HeaderMap) -> bool {
        let Some(sid) = cookie(headers, COOKIE) else { return false };
        self.ui_sessions.lock().unwrap_or_else(|p| p.into_inner()).contains(&sid)
    }

    fn claude(&self) -> ClaudeConnector {
        ClaudeConnector { state_dir: self.agenttracker_home.join("connectors"), ..ClaudeConnector::for_user() }
    }

    fn cursor(&self) -> CursorConnector {
        CursorConnector { state_dir: self.agenttracker_home.join("connectors"), ..CursorConnector::for_user() }
    }

    fn copilot(&self) -> CopilotConnector {
        CopilotConnector { state_dir: self.agenttracker_home.join("connectors"), ..CopilotConnector::for_user() }
    }

    pub fn setting(&self, key: &str) -> String {
        self.engine.db.get_setting(key).ok().flatten().unwrap_or_else(|| SETTING_DEFAULTS.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()).unwrap_or_default())
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn hdr<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn j(status: u16, v: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), Json(v)).into_response()
}

fn err(status: u16, msg: &str) -> Response {
    j(status, json!({"error": msg}))
}

fn core_err(e: CoreError) -> Response {
    match e {
        CoreError::NotFound(m) => err(404, &format!("Not found: {m}")),
        CoreError::Invalid(m) => err(400, &m),
        e @ CoreError::Lifecycle { .. } => err(409, &e.to_string()),
        e => err(500, &e.to_string()),
    }
}

fn ok<T: serde::Serialize>(r: crate::Result<T>) -> Response {
    match r {
        Ok(v) => j(200, serde_json::to_value(v).unwrap_or(Value::Null)),
        Err(e) => core_err(e),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.map_err(|e| err(500, &format!("worker failed: {e}")))
}

// ---- guards --------------------------------------------------------------------------------------
async fn guards(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let host = hdr(req.headers(), "host").unwrap_or("");
    if !security::host_allowed(host, st.port) {
        return err(403, "Host not allowed");
    }
    if path.starts_with("/api/ui/") {
        if !st.has_session(req.headers()) {
            return err(401, "Open AgentTracker from its window or tray icon");
        }
        if req.method() != Method::GET && req.method() != Method::HEAD && hdr(req.headers(), CSRF_HEADER) != Some(CSRF_VALUE) {
            return err(403, "Missing CSRF header");
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("cache-control", HeaderValue::from_static("no-store"));
    if path == "/" || path.starts_with("/static/") {
        h.insert("content-security-policy", HeaderValue::from_static(CSP));
    }
    resp
}

pub fn build_router(state: Arc<AppState>) -> Router {
    build_router_with(state, Router::new())
}

/// Builds the router with additional UI routes (e.g. the decision runtime's `/api/ui/runtime/*`).
/// Extra routes are merged before the guard layer, so they get the same session/CSRF/host checks.
pub fn build_router_with(state: Arc<AppState>, extra: Router<Arc<AppState>>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/*name", get(static_file))
        .route("/launch", get(launch))
        .route("/api/health", get(|| async { j(200, json!({"app": "agenttracker", "version": VERSION})) }))
        .route("/ingest", post(ingest))
        .route("/api/ui/state", get(ui_state))
        .route("/api/ui/pause", post(ui_pause))
        .route("/api/ui/connectors", get(connectors_list))
        .route("/api/ui/connectors/:name/:action", post(connector_action).get(connector_preview))
        .route("/api/ui/status", get(ui_status))
        .route("/api/ui/collection/:action", post(collection_action))
        .route("/api/ui/settings/save", post(settings_save))
        .route("/api/ui/retention/purge", post(retention_purge))
        .route("/api/ui/pick-folder", post(pick_folder))
        .route("/api/ui/runs/:id", get(run_get))
        .route("/api/ui/export/:fmt/:id", post(run_export))
        .route("/api/ui/incident/propose", post(incident_propose))
        .route("/api/ui/tasks/:id/live", get(task_live))
        .route("/api/ui/check-types", get(check_types))
        .route("/api/ui/tasks", get(tasks_list).post(task_create))
        .route("/api/ui/tasks/:id", get(task_get))
        .route("/api/ui/tasks/:id/checks", put(task_checks))
        .route("/api/ui/tasks/:id/:action", post(task_action))
        .route("/api/ui/events/recent", get(events_recent))
        .route("/api/ui/templates", get(templates_list).post(template_create))
        .route("/api/ui/templates/:id", delete(template_delete).put(template_rename))
        .route("/api/ui/settings", get(settings_get))
        .route("/api/ui/license", post(license_set))
        .route("/api/ui/chain/verify", post(chain_verify))
        .route("/api/ui/demo", post(demo_create))
        .merge(extra)
        .fallback(|| async { err(404, "Not found") })
        .layer(middleware::from_fn_with_state(state.clone(), guards))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    axum::serve(listener, build_router(state)).await
}

pub async fn serve_with(listener: tokio::net::TcpListener, state: Arc<AppState>, extra: Router<Arc<AppState>>) -> std::io::Result<()> {
    axum::serve(listener, build_router_with(state, extra)).await
}

// ---- static + launch -------------------------------------------------------------------------------
const INDEX_HTML: &[u8] = include_bytes!("../../../static/index.html");

fn static_asset(name: &str) -> Option<(&'static [u8], &'static str)> {
    Some(match name {
        "index.html" => (INDEX_HTML, "text/html; charset=utf-8"),
        "app.js" => (include_bytes!("../../../static/app.js"), "text/javascript; charset=utf-8"),
        "style.css" => (include_bytes!("../../../static/style.css"), "text/css; charset=utf-8"),
        "runtime.js" => (include_bytes!("../../../static/runtime.js"), "text/javascript; charset=utf-8"),
        "icon.svg" => (include_bytes!("../../../static/icon.svg"), "image/svg+xml"),
        _ => return None,
    })
}

async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML).into_response()
}

async fn static_file(UrlPath(name): UrlPath<String>) -> Response {
    match static_asset(&name) {
        Some((data, mime)) => ([(header::CONTENT_TYPE, mime)], data).into_response(),
        None => err(404, "Not found"),
    }
}

async fn launch(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let token = q.get("token").cloned().unwrap_or_default();
    let valid = {
        let mut lt = st.launch_token.lock().unwrap_or_else(|p| p.into_inner());
        let valid = lt.as_deref().map(|t| !t.is_empty() && security::ct_eq(token.as_bytes(), t.as_bytes())).unwrap_or(false);
        if valid {
            *lt = None; // one-time
        }
        valid
    };
    if !valid {
        return (StatusCode::FORBIDDEN, "This link has expired. Open AgentTracker from its window or tray icon.").into_response();
    }
    let sid = security::token_urlsafe(32);
    st.ui_sessions.lock().unwrap_or_else(|p| p.into_inner()).insert(sid.clone());
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "/")
        .header(header::SET_COOKIE, format!("{COOKIE}={sid}; HttpOnly; Path=/; SameSite=Strict"))
        .body(Body::empty())
        .unwrap()
}

// ---- hook ingest -------------------------------------------------------------------------------------
async fn ingest(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let now = crate::now_ms() / 1000;
    if let Err(e) = security::verify_signature(&st.hook_secret, hdr(&headers, security::HEADER_TIMESTAMP), hdr(&headers, security::HEADER_SIGNATURE), &body, now) {
        return err(401, e);
    }
    match st.collector.accept(&body) {
        Ok(id) => j(202, json!({"queued": id, "paused": st.collector.is_paused()})),
        Err(e) => core_err(e),
    }
}

// ---- UI: state / settings ----------------------------------------------------------------------------
async fn ui_state(State(st): State<Arc<AppState>>) -> Response {
    let key = st.engine.db.get_setting("license_key").ok().flatten();
    let mut values = serde_json::Map::new();
    for (k, _) in SETTING_DEFAULTS {
        values.insert(k.to_string(), Value::String(st.setting(k)));
    }
    j(
        200,
        json!({
            "values": values,
            "app": APP_NAME,
            "version": VERSION,
            "paused": st.collector.is_paused(),
            "event_count": st.engine.events.count().unwrap_or(0),
            "queue_pending": st.collector.queue.pending_count().unwrap_or(0),
            "hook_binary": st.hook_binary.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "hook_binary_exists": st.hook_binary.as_ref().map(|p| p.exists()).unwrap_or(false),
            "license": license::status_for(key.as_deref()),
            "data_dir": st.data_dir.to_string_lossy(),
        }),
    )
}

async fn ui_pause(State(st): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let p = body.get("paused").and_then(Value::as_bool).unwrap_or(!st.collector.is_paused());
    st.collector.set_paused(p);
    let _ = st.engine.db.set_setting("paused", if p { "1" } else { "0" });
    j(200, json!({"paused": p}))
}

async fn settings_get(State(st): State<Arc<AppState>>) -> Response {
    let key = st.engine.db.get_setting("license_key").ok().flatten();
    let mut values = serde_json::Map::new();
    for (k, _) in SETTING_DEFAULTS {
        values.insert(k.to_string(), Value::String(st.setting(k)));
    }
    j(
        200,
        json!({
            "values": values,
            "paused": st.collector.is_paused(),
            "license": license::status_for(key.as_deref()),
            "data_dir": st.data_dir.to_string_lossy(),
            "agenttracker_home": st.agenttracker_home.to_string_lossy(),
            "hook_binary": st.hook_binary.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "machine_id_prefix": &tracker_common::machine_id::machine_fingerprint()[..12],
            "version": VERSION,
        }),
    )
}

async fn license_set(State(st): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let key = body.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let status = license::status_for(Some(&key));
    if status.state == license::LicenseState::FormatValid {
        if let Err(e) = st.engine.db.set_setting("license_key", &key.to_ascii_uppercase()) {
            return core_err(e);
        }
    }
    j(200, serde_json::to_value(status).unwrap_or(Value::Null))
}

async fn chain_verify(State(st): State<Arc<AppState>>) -> Response {
    let engine = st.engine.clone();
    match blocking(move || -> crate::Result<Value> {
        let chain = engine.events.chain()?;
        let n = chain.len()?;
        Ok(serde_json::to_value(chain.verify_range(1, n.max(1))?)?)
    })
    .await
    {
        Ok(r) => ok(r),
        Err(resp) => resp,
    }
}

// ---- connectors ------------------------------------------------------------------------------------
async fn connectors_list(State(st): State<Arc<AppState>>) -> Response {
    let s = st.clone();
    let r = blocking(move || {
        let recent = s.engine.events.recent(300).unwrap_or_default();
        let day_ago = crate::now_ms() - 24 * 3600 * 1000;
        let hook_ok = s.hook_binary.as_ref().map(|p| p.exists()).unwrap_or(false);
        let entry = |info: crate::connectors::ConnectorInfo, det: DetectResult, caps: Vec<crate::connectors::Capability>| {
            let errs: Vec<_> = recent.iter().filter(|e| e.connector_name == info.name && e.event_type == crate::event::EventType::CaptureError && e.collector_ts > day_ago).collect();
            let unsupported = errs.iter().any(|e| e.payload_redacted.get(crate::connectors::UNSUPPORTED_MARKER).is_some());
            let (state, state_note) = crate::connectors::compute_state(&det, hook_ok, errs.len(), unsupported);
            let health = s.collector.health.get(info.name).ok().flatten();
            json!({"info": info, "detect": det, "capabilities": caps, "health": health, "state": state, "state_note": state_note})
        };
        json!([
            entry(crate::connectors::claude_code::info(), s.claude().detect(), ClaudeConnector::capabilities()),
            entry(crate::connectors::cursor::info(), s.cursor().detect(), CursorConnector::capabilities()),
            entry(crate::connectors::copilot::info(), s.copilot().detect(), CopilotConnector::capabilities()),
            {"info": crate::connectors::manual::info(), "detect": ManualConnector::detect(), "capabilities": ManualConnector::capabilities(), "health": Value::Null, "state": "connected", "state_note": crate::connectors::manual::LABEL},
        ])
    })
    .await;
    match r {
        Ok(v) => j(200, v),
        Err(resp) => resp,
    }
}

async fn connector_preview(State(st): State<Arc<AppState>>, UrlPath((name, action)): UrlPath<(String, String)>) -> Response {
    if action != "preview" {
        return err(404, "Not found");
    }
    let s = st.clone();
    let r = blocking(move || -> crate::Result<Value> {
        // Preview works even without the hook program, using the path it would be installed at.
        let hook = s.hook_binary.clone().unwrap_or_else(|| PathBuf::from("agenttracker-hook"));
        Ok(match name.as_str() {
            "claude_code" => serde_json::to_value(s.claude().preview(&hook)?)?,
            "cursor" => serde_json::to_value(s.cursor().preview(&hook)?)?,
            "copilot_vscode" => serde_json::to_value(s.copilot().preview(&hook)?)?,
            _ => return Err(CoreError::NotFound(name)),
        })
    })
    .await;
    match r {
        Ok(v) => ok(v),
        Err(resp) => resp,
    }
}

/// End-to-end self check in a throwaway folder: snapshot → change a file → verify → clean up.
/// Proves the file-verification pipeline works on this computer without touching user files.
fn pipeline_selftest(engine: &crate::engine::Engine, data_dir: &std::path::Path) -> crate::connectors::TestStep {
    let run = || -> crate::Result<bool> {
        let dir = data_dir.join(format!("selftest-{}", crate::now_ms()));
        std::fs::create_dir_all(dir.join("out"))?;
        std::fs::write(dir.join("keep.txt"), "original")?;
        let s = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        let t = engine.tasks.create(NewTask {
            title: "Connection self-test".into(),
            instruction: String::new(),
            connector: "manual".into(),
            folders: vec![
                crate::task::TaskFolder { folder_path: s(dir.clone()), role: crate::task::FolderRole::Working },
                crate::task::TaskFolder { folder_path: s(dir.join("keep.txt")), role: crate::task::FolderRole::Protected },
            ],
            checks: vec![NewCheck { check_type: "required_outputs_exist".into(), params: json!({"paths": ["out/hello.txt"]}), is_required: true }],
        })?;
        let result = (|| -> crate::Result<bool> {
            engine.capture_baseline(&t.id)?;
            engine.start(&t.id)?;
            std::fs::write(dir.join("out").join("hello.txt"), "hello")?;
            let run = engine.verify(&t.id)?;
            Ok(run.overall_verdict == Some(crate::verifiers::CheckStatus::Passed))
        })();
        let _ = engine.cancel(&t.id);
        let _ = engine.db.delete_task_data(&t.id);
        let _ = std::fs::remove_dir_all(&dir);
        result
    };
    match run() {
        Ok(true) => crate::connectors::TestStep { name: "File checking".into(), ok: true, detail: "A practice task in a temporary folder was snapshotted, changed and verified correctly, then removed.".into() },
        Ok(false) => crate::connectors::TestStep { name: "File checking".into(), ok: false, detail: "The practice task in a temporary folder didn't verify as expected.".into() },
        Err(e) => crate::connectors::TestStep { name: "File checking".into(), ok: false, detail: format!("The practice task failed: {e}") },
    }
}

async fn connector_action(State(st): State<Arc<AppState>>, UrlPath((name, action)): UrlPath<(String, String)>) -> Response {
    let s = st.clone();
    let r = blocking(move || -> crate::Result<Value> {
        let hook = s.hook_binary.clone();
        let need_hook = || hook.clone().filter(|p| p.exists()).ok_or_else(|| CoreError::Invalid("The AgentTracker hook program wasn't found next to the app. Reinstall AgentTracker.".into()));
        let mut v = match (name.as_str(), action.as_str()) {
            ("claude_code", "install") => serde_json::to_value(s.claude().install(&need_hook()?)?)?,
            ("claude_code", "uninstall") => serde_json::to_value(s.claude().uninstall()?)?,
            ("claude_code", "test") => serde_json::to_value(s.claude().connection_test())?,
            ("cursor", "install") => serde_json::to_value(s.cursor().install(&need_hook()?)?)?,
            ("cursor", "uninstall") => serde_json::to_value(s.cursor().uninstall()?)?,
            ("cursor", "test") => serde_json::to_value(s.cursor().connection_test())?,
            ("copilot_vscode", "install") => serde_json::to_value(s.copilot().install(&need_hook()?)?)?,
            ("copilot_vscode", "uninstall") => serde_json::to_value(s.copilot().uninstall()?)?,
            ("copilot_vscode", "test") => serde_json::to_value(s.copilot().connection_test())?,
            ("manual", "test") => json!({"ok": true, "steps": [{"name": "Manual", "ok": true, "detail": crate::connectors::manual::LABEL}]}),
            _ => return Err(CoreError::NotFound(format!("{name}/{action}"))),
        };
        // Every connection test also runs the harmless temp-folder pipeline check.
        if action == "test" {
            if let Some(steps) = v.get_mut("steps").and_then(Value::as_array_mut) {
                steps.push(serde_json::to_value(pipeline_selftest(&s.engine, &s.data_dir))?);
            }
            let all_ok = v.get("steps").and_then(Value::as_array).map(|a| a.iter().all(|x| x.get("ok") == Some(&Value::Bool(true)))).unwrap_or(false);
            v["ok"] = Value::Bool(all_ok);
        }
        Ok(v)
    })
    .await;
    match r {
        Ok(v) => ok(v),
        Err(resp) => resp,
    }
}

// ---- tasks ---------------------------------------------------------------------------------------------
async fn check_types(State(st): State<Arc<AppState>>) -> Response {
    let v: Vec<Value> = st.engine.registry.types().into_iter().map(|(t, v)| json!({"check_type": t, "version": v})).collect();
    j(200, json!(v))
}

async fn tasks_list(State(st): State<Arc<AppState>>) -> Response {
    ok(st.engine.tasks.list())
}

async fn task_create(State(st): State<Arc<AppState>>, Json(body): Json<NewTask>) -> Response {
    ok(st.engine.tasks.create(body))
}

async fn task_get(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let e = &st.engine;
    let r = (|| -> crate::Result<Value> {
        let task = e.tasks.get(&id)?;
        let mut events = e.events.by_task(&id)?;
        if let Some(sid) = &task.session_id {
            for ev in e.events.by_session(sid)? {
                if !events.iter().any(|x| x.event_id == ev.event_id) {
                    events.push(ev);
                }
            }
        }
        crate::event::sort_source_order(&mut events);
        let baselines: Vec<Value> = e
            .manifests
            .latest_for_task(&id, crate::fs_observer::ManifestType::Baseline)?
            .iter()
            .map(|m| {
                json!({"root": m.root, "captured_at": m.captured_at, "files": m.files().count(),
                    "unreadable": m.entries.iter().filter(|x| !x.read_ok).count(),
                    "unstable": m.entries.iter().filter(|x| x.stability == crate::fs_observer::Stability::Unstable).count()})
            })
            .collect();
        Ok(json!({
            "task": task,
            "events": events,
            "runs": e.results.runs_for_task(&id)?,
            "baselines": baselines,
            "live_file_events": e.live_hint_count(&id),
        }))
    })();
    ok(r)
}

async fn task_checks(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>, Json(checks): Json<Vec<NewCheck>>) -> Response {
    ok(st.engine.tasks.update_checks(&id, checks))
}

async fn task_action(State(st): State<Arc<AppState>>, UrlPath((id, action)): UrlPath<(String, String)>, body: Bytes) -> Response {
    let engine = st.engine.clone();
    let key = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("idempotency_key").and_then(Value::as_str).map(str::to_string));
    let r = blocking(move || -> crate::Result<Value> {
        Ok(match action.as_str() {
            "baseline" => serde_json::to_value(engine.capture_baseline(&id)?)?,
            "start" => serde_json::to_value(engine.start(&id)?)?,
            "verify" => serde_json::to_value(engine.verify_with_key(&id, key.as_deref())?)?,
            "cancel" => serde_json::to_value(engine.cancel(&id)?)?,
            _ => return Err(CoreError::NotFound(action)),
        })
    })
    .await;
    match r {
        Ok(v) => ok(v),
        Err(resp) => resp,
    }
}

async fn events_recent(State(st): State<Arc<AppState>>) -> Response {
    ok(st.engine.events.recent(100))
}

// ---- templates ---------------------------------------------------------------------------------------------
async fn templates_list(State(st): State<Arc<AppState>>) -> Response {
    ok(st.engine.templates.list())
}

async fn template_create(State(st): State<Arc<AppState>>, Json(b): Json<Value>) -> Response {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let ct = b.get("check_type").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() || st.engine.registry.get(ct).is_none() {
        return err(400, "A saved check needs a name and a known check type");
    }
    ok(st.engine.templates.create(name, ct, b.get("params").unwrap_or(&Value::Null), "user"))
}

async fn template_delete(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    ok(st.engine.templates.delete(&id))
}

// ---- demo ---------------------------------------------------------------------------------------------
async fn demo_create(State(st): State<Arc<AppState>>) -> Response {
    let r = (|| -> crate::Result<Value> {
        let (task, dir) = crate::demo::install_demo(&st.data_dir.join("demo"))?;
        let t = st.engine.tasks.create(task)?;
        Ok(json!({"task": t, "folder": dir.to_string_lossy()}))
    })();
    ok(r)
}

// ---- Phase 3 UI endpoints -----------------------------------------------------------------------------
async fn ui_status(State(st): State<Arc<AppState>>) -> Response {
    let tasks = st.engine.tasks.list().unwrap_or_default();
    let active = tasks.iter().filter(|t| matches!(t.lifecycle, crate::task::Lifecycle::Running | crate::task::Lifecycle::Ready | crate::task::Lifecycle::Baselining | crate::task::Lifecycle::Verifying)).count();
    let sessions: HashSet<String> = st.engine.events.recent(300).unwrap_or_default().into_iter().filter_map(|e| e.session_id).collect();
    j(
        200,
        json!({
            "collecting": !st.collector.is_paused(),
            "paused": st.collector.is_paused(),
            "task_count": tasks.len(),
            "active_task_count": active,
            "session_count": sessions.len(),
            "event_count": st.engine.events.count().unwrap_or(0),
            "queue_pending": st.collector.queue.pending_count().unwrap_or(0),
            "hook_binary_exists": st.hook_binary.as_ref().map(|p| p.exists()).unwrap_or(false),
            "wizard_done": st.setting("wizard_done") == "1",
            "work_mode": st.setting("work_mode"),
            "folder_picker": st.folder_picker.get().is_some(),
            "version": VERSION,
        }),
    )
}

async fn collection_action(State(st): State<Arc<AppState>>, UrlPath(action): UrlPath<String>) -> Response {
    let p = match action.as_str() {
        "pause" => true,
        "resume" => false,
        _ => return err(404, "Not found"),
    };
    st.collector.set_paused(p);
    let _ = st.engine.db.set_setting("paused", if p { "1" } else { "0" });
    j(200, json!({"paused": p, "collecting": !p}))
}

/// Applies "Start with Windows" via the per-user Run key. Windows only; untested in CI here.
fn apply_autostart(enabled: bool) -> std::result::Result<(), String> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut cmd = crate::connectors::hidden_command("reg");
        if enabled {
            cmd.args(["add", key, "/v", "AgentTracker", "/t", "REG_SZ", "/d", &format!("\"{}\"", exe.display()), "/f"]);
        } else {
            cmd.args(["delete", key, "/v", "AgentTracker", "/f"]);
        }
        let out = cmd.output().map_err(|e| e.to_string())?;
        if !enabled || out.status.success() {
            return Ok(());
        }
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("Start with Windows is only available on Windows.".into())
    }
}

async fn settings_save(State(st): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let Some(obj) = body.as_object() else { return err(400, "Expected an object") };
    let mut notes = vec![];
    for (k, v) in obj {
        if !SETTING_DEFAULTS.iter().any(|(d, _)| d == k) {
            return err(400, &format!("Unknown setting {k}"));
        }
        let val = match v {
            Value::Bool(b) => if *b { "1".to_string() } else { "0".to_string() },
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => return err(400, &format!("Bad value for {k}")),
        };
        if k == "retention_days" && val.parse::<i64>().map(|n| n < 0).unwrap_or(true) {
            return err(400, "Retention must be a number of days (0 = keep forever)");
        }
        if k == "start_with_windows" {
            if let Err(e) = apply_autostart(val == "1") {
                notes.push(e);
            }
        }
        if k == "redaction_patterns" {
            st.collector.set_custom_redactions(val.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect());
        }
        if let Err(e) = st.engine.db.set_setting(k, &val) {
            return core_err(e);
        }
    }
    j(200, json!({"saved": true, "notes": notes}))
}

async fn retention_purge(State(st): State<Arc<AppState>>) -> Response {
    let days: i64 = st.setting("retention_days").parse().unwrap_or(0);
    if days <= 0 {
        return j(200, json!({"tasks_deleted": 0, "events_deleted": 0, "kept_active": [], "note": "Retention is set to keep everything."}));
    }
    let engine = st.engine.clone();
    match blocking(move || engine.purge_older_than(crate::now_ms() - days * 24 * 3600 * 1000)).await {
        Ok(r) => ok(r),
        Err(resp) => resp,
    }
}

async fn pick_folder(State(st): State<Arc<AppState>>) -> Response {
    let Some(picker) = st.folder_picker.get().cloned() else {
        return j(200, json!({"supported": false}));
    };
    match blocking(move || picker()).await {
        Ok(path) => j(200, json!({"supported": true, "path": path})),
        Err(resp) => resp,
    }
}

/// Loads a run plus the frozen task definition it was checked against and the agent events.
fn load_run(engine: &crate::engine::Engine, run_id: &str) -> crate::Result<(crate::task::TaskDefinition, crate::store::VerificationRun, Vec<crate::event::AgentEvent>)> {
    let run = engine.results.get_run(run_id)?.ok_or_else(|| CoreError::NotFound(format!("run {run_id}")))?;
    let current = engine.tasks.get(&run.task_id)?;
    let task = engine.tasks.snapshot(&run.task_id, run.definition_version)?.unwrap_or(current);
    let mut events = engine.events.by_task(&run.task_id)?;
    if let Some(sid) = &task.session_id {
        for e in engine.events.by_session(sid)? {
            if !events.iter().any(|x| x.event_id == e.event_id) {
                events.push(e);
            }
        }
    }
    crate::event::sort_source_order(&mut events);
    Ok((task, run, events))
}

async fn run_get(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    ok(load_run(&st.engine, &id).map(|(task, run, events)| json!({"task": task, "run": run, "events": events})))
}

async fn run_export(State(st): State<Arc<AppState>>, UrlPath((fmt, id)): UrlPath<(String, String)>) -> Response {
    let (task, run, events) = match load_run(&st.engine, &id) {
        Ok(x) => x,
        Err(e) => return core_err(e),
    };
    let stamp = &run.id[..8.min(run.id.len())];
    let (body, mime, ext) = match fmt.as_str() {
        "html" => (crate::report::export_html(&task, &run, &events), "text/html; charset=utf-8", "html"),
        "json" => (serde_json::to_string_pretty(&crate::report::export_json(&task, &run, &events)).unwrap_or_default(), "application/json", "json"),
        _ => return err(404, "Unknown export format"),
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"agenttracker-report-{stamp}.{ext}\""))
        .body(Body::from(body))
        .unwrap()
}

async fn incident_propose(State(st): State<Arc<AppState>>, Json(b): Json<Value>) -> Response {
    let run_id = b.get("run_id").and_then(Value::as_str).unwrap_or("");
    let check_id = b.get("check_id").and_then(Value::as_str).unwrap_or("");
    let (task, run, _) = match load_run(&st.engine, run_id) {
        Ok(x) => x,
        Err(e) => return core_err(e),
    };
    let Some(r) = run.results.iter().find(|r| r.check_id == check_id) else { return err(404, "That check isn't part of this run") };
    j(200, serde_json::to_value(crate::report::propose_from_incident(&task, r)).unwrap_or(Value::Null))
}

async fn template_rename(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>, Json(b): Json<Value>) -> Response {
    let name = b.get("name").and_then(Value::as_str).unwrap_or("").trim();
    if name.is_empty() {
        return err(400, "Please give the saved check a name");
    }
    match st.engine.templates.rename(&id, name) {
        Ok(true) => j(200, json!({"renamed": true})),
        Ok(false) => err(404, "Saved check not found"),
        Err(e) => core_err(e),
    }
}

/// Live Task: agent-reported activity, independently observed changes and capture gaps, kept apart.
async fn task_live(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let e = st.engine.clone();
    let r = blocking(move || -> crate::Result<Value> {
        let task = e.tasks.get(&id)?;
        let mut events = e.events.by_task(&id)?;
        if let Some(sid) = &task.session_id {
            for ev in e.events.by_session(sid)? {
                if !events.iter().any(|x| x.event_id == ev.event_id) {
                    events.push(ev);
                }
            }
        }
        crate::event::sort_source_order(&mut events);
        let view = e.live_view(&id);
        let mut gaps: Vec<Value> = view.gaps.iter().map(|g| json!({"kind": "observer", "detail": g})).collect();
        let (errs, agent): (Vec<_>, Vec<_>) = events.into_iter().partition(|x| x.event_type == crate::event::EventType::CaptureError);
        for x in &errs {
            gaps.push(json!({"kind": "capture_error", "detail": x.payload_redacted.get("error").and_then(Value::as_str).unwrap_or("A message from the agent couldn't be understood"), "ts": x.collector_ts}));
        }
        for m in e.manifests.latest_for_task(&id, crate::fs_observer::ManifestType::Baseline)? {
            let unreadable = m.entries.iter().filter(|x| !x.read_ok).count();
            if unreadable > 0 {
                gaps.push(json!({"kind": "unreadable", "detail": format!("{unreadable} file(s) in {} couldn't be read when the snapshot was taken", m.root)}));
            }
        }
        if st.collector.is_paused() {
            gaps.push(json!({"kind": "paused", "detail": "Collection is paused — agent messages are queued but not recorded until you resume."}));
        }
        let agent_events: Vec<Value> = agent.iter().map(|x| json!({"event": x, "label": crate::report::event_label(x.event_type)})).collect();
        Ok(json!({"task": task, "agent_events": agent_events, "observed": view, "gaps": gaps, "manual": task.connector == "manual"}))
    })
    .await;
    match r {
        Ok(v) => ok(v),
        Err(resp) => resp,
    }
}
