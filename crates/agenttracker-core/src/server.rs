//! Loopback-only HTTP server: the UI API (one-time launch token → HttpOnly session cookie + CSRF
//! header) and the hook ingest endpoint (HMAC-signed). Same pattern as AI Tracker Lite.
use crate::collector::Collector;
use crate::connectors::{claude_code::ClaudeConnector, cursor::CursorConnector, manual::ManualConnector};
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
use std::sync::{Arc, Mutex};
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
}

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
    Router::new()
        .route("/", get(index))
        .route("/static/*name", get(static_file))
        .route("/launch", get(launch))
        .route("/api/health", get(|| async { j(200, json!({"app": "agenttracker", "version": VERSION})) }))
        .route("/ingest", post(ingest))
        .route("/api/ui/state", get(ui_state))
        .route("/api/ui/pause", post(ui_pause))
        .route("/api/ui/connectors", get(connectors_list))
        .route("/api/ui/connectors/:name/:action", post(connector_action))
        .route("/api/ui/check-types", get(check_types))
        .route("/api/ui/tasks", get(tasks_list).post(task_create))
        .route("/api/ui/tasks/:id", get(task_get))
        .route("/api/ui/tasks/:id/checks", put(task_checks))
        .route("/api/ui/tasks/:id/:action", post(task_action))
        .route("/api/ui/events/recent", get(events_recent))
        .route("/api/ui/templates", get(templates_list).post(template_create))
        .route("/api/ui/templates/:id", delete(template_delete))
        .route("/api/ui/settings", get(settings_get))
        .route("/api/ui/license", post(license_set))
        .route("/api/ui/chain/verify", post(chain_verify))
        .route("/api/ui/demo", post(demo_create))
        .fallback(|| async { err(404, "Not found") })
        .layer(middleware::from_fn_with_state(state.clone(), guards))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    axum::serve(listener, build_router(state)).await
}

// ---- static + launch -------------------------------------------------------------------------------
const INDEX_HTML: &[u8] = include_bytes!("../../../static/index.html");

fn static_asset(name: &str) -> Option<(&'static [u8], &'static str)> {
    Some(match name {
        "index.html" => (INDEX_HTML, "text/html; charset=utf-8"),
        "app.js" => (include_bytes!("../../../static/app.js"), "text/javascript; charset=utf-8"),
        "style.css" => (include_bytes!("../../../static/style.css"), "text/css; charset=utf-8"),
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
    j(
        200,
        json!({
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
    j(
        200,
        json!({
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
        let (claude, cursor) = (s.claude(), s.cursor());
        let health = |n: &str| s.collector.health.get(n).ok().flatten();
        json!([
            {"info": crate::connectors::claude_code::info(), "detect": claude.detect(), "capabilities": ClaudeConnector::capabilities(), "health": health("claude_code")},
            {"info": crate::connectors::cursor::info(), "detect": cursor.detect(), "capabilities": CursorConnector::capabilities(), "health": health("cursor")},
            {"info": crate::connectors::manual::info(), "detect": ManualConnector::detect(), "capabilities": ManualConnector::capabilities(), "health": Value::Null},
        ])
    })
    .await;
    match r {
        Ok(v) => j(200, v),
        Err(resp) => resp,
    }
}

async fn connector_action(State(st): State<Arc<AppState>>, UrlPath((name, action)): UrlPath<(String, String)>) -> Response {
    let s = st.clone();
    let r = blocking(move || -> crate::Result<Value> {
        let hook = s.hook_binary.clone();
        let need_hook = || hook.clone().filter(|p| p.exists()).ok_or_else(|| CoreError::Invalid("The AgentTracker hook program wasn't found next to the app. Reinstall AgentTracker.".into()));
        Ok(match (name.as_str(), action.as_str()) {
            ("claude_code", "install") => serde_json::to_value(s.claude().install(&need_hook()?)?)?,
            ("claude_code", "uninstall") => serde_json::to_value(s.claude().uninstall()?)?,
            ("claude_code", "test") => serde_json::to_value(s.claude().connection_test())?,
            ("cursor", "install") => serde_json::to_value(s.cursor().install(&need_hook()?)?)?,
            ("cursor", "uninstall") => serde_json::to_value(s.cursor().uninstall()?)?,
            ("cursor", "test") => serde_json::to_value(s.cursor().connection_test())?,
            ("manual", "test") => json!({"ok": true, "steps": [{"name": "Manual", "ok": true, "detail": crate::connectors::manual::LABEL}]}),
            _ => return Err(CoreError::NotFound(format!("{name}/{action}"))),
        })
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

async fn task_action(State(st): State<Arc<AppState>>, UrlPath((id, action)): UrlPath<(String, String)>) -> Response {
    let engine = st.engine.clone();
    let r = blocking(move || -> crate::Result<Value> {
        Ok(match action.as_str() {
            "baseline" => serde_json::to_value(engine.capture_baseline(&id)?)?,
            "start" => serde_json::to_value(engine.start(&id)?)?,
            "verify" => serde_json::to_value(engine.verify(&id)?)?,
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
