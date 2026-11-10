//! UI routes for the runtime, mounted by the desktop shell under `/api/ui/runtime/*`.
//! They inherit the core server's guards (session cookie + CSRF header + host check).
//! Replay-vault access is a separate, explicit action (investigations / delete), never part of the
//! ordinary record views.
use crate::config::RuntimeConfig;
use crate::demo::{self, Demo};
use crate::investigate::{self, InvestigationSpec};
use crate::provider::{OpenAiCompatProvider, Provider};
use crate::replay_vault::{self, ReplayVault};
use crate::runtime::{Runtime, TaskSpec};
use crate::relay::{self, RelayConfig};
use crate::{checks, store, why, RtError};
use agenttracker_core::db::Db;
use agenttracker_core::server::AppState;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

type St = State<Arc<AppState>>;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/ui/runtime/config", get(config_get).post(config_set))
        .route("/api/ui/runtime/provider/test", post(provider_test))
        .route("/api/ui/runtime/provider/models", get(provider_models))
        .route("/api/ui/runtime/sessions", get(sessions_list))
        .route("/api/ui/runtime/sessions/:id", get(session_get))
        .route("/api/ui/runtime/sessions/:id/resume", post(session_resume))
        .route("/api/ui/runtime/sessions/:id/retry", post(session_retry))
        .route("/api/ui/runtime/tasks", post(task_start))
        .route("/api/ui/runtime/decisions/:id/approve", post(decision_approve))
        .route("/api/ui/runtime/decisions/:id/why", get(decision_why))
        .route("/api/ui/runtime/decisions/:id/analyze", post(decision_analyze))
        .route("/api/ui/runtime/actions/:id/why", get(action_why))
        .route("/api/ui/runtime/investigations", post(investigation_start))
        .route("/api/ui/runtime/investigations/:id", get(investigation_get))
        .route("/api/ui/runtime/demo/:which", post(demo_run))
        .route("/api/ui/runtime/replay/delete", post(replay_delete))
        .route("/api/ui/runtime/recover", post(recover))
        .route("/api/ui/runtime/relay", get(relay_get).post(relay_set))
        .route("/api/ui/runtime/relay/test", post(relay_test))
        .route("/api/ui/runtime/relay/conversations", get(relay_conversations))
        .route("/api/ui/runtime/relay/conversations/:id", get(relay_conversation))
        .route("/api/ui/runtime/relay/delete", post(relay_delete))
}

fn j(status: u16, v: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), Json(v)).into_response()
}

fn rt_err(e: RtError) -> Response {
    match e {
        RtError::NotFound(m) => j(404, json!({"error": format!("Not found: {m}")})),
        RtError::Invalid(m) | RtError::Refused(m) => j(400, json!({"error": m})),
        RtError::Provider(m) => j(502, json!({"error": format!("Model provider: {m}")})),
        e => j(500, json!({"error": tracker_common::redaction::redact_text(&e.to_string())})),
    }
}

fn reply(r: crate::Result<Value>) -> Response {
    match r {
        Ok(v) => j(200, v),
        Err(e) => rt_err(e),
    }
}

async fn blocking(f: impl FnOnce() -> crate::Result<Value> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => reply(r),
        Err(e) => j(500, json!({"error": format!("worker failed: {e}")})),
    }
}

pub fn load_config(db: &Db) -> RuntimeConfig {
    let mut c: RuntimeConfig = db.get_setting("rt_config").ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    c.provider.api_key = db.get_setting("rt_api_key").ok().flatten().filter(|k| !k.is_empty());
    c
}

fn vault(st: &AppState, cfg: &RuntimeConfig) -> Option<Arc<ReplayVault>> {
    if !cfg.replay_capture {
        return None;
    }
    ReplayVault::open(&replay_vault::vault_dir(&st.agenttracker_home)).ok().map(Arc::new)
}

fn live_runtime(st: &AppState) -> crate::Result<Runtime> {
    live_runtime_with_model(st, None)
}

/// Like `live_runtime`, but `model` (when given) replaces the saved model for this runtime only.
/// The saved provider settings are not changed.
fn live_runtime_with_model(st: &AppState, model: Option<&str>) -> crate::Result<Runtime> {
    let mut cfg = load_config(&st.engine.db);
    if let Some(m) = model {
        cfg.provider.model = m.to_string();
    }
    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatProvider::new(cfg.provider.clone())?);
    let v = vault(st, &cfg);
    Ok(Runtime::new(st.engine.db.clone(), provider, cfg, v))
}

async fn config_get(State(st): St) -> Response {
    let c = load_config(&st.engine.db);
    let key_protection = if cfg!(windows) { "Windows DPAPI (current user)" } else { "file (mode 0600, not OS-encrypted)" };
    j(200, json!({"config": c, "has_api_key": c.provider.api_key.is_some(), "runtime_version": crate::RUNTIME_VERSION, "replay_key_protection": key_protection}))
}

#[derive(Deserialize)]
struct ConfigIn {
    config: RuntimeConfig,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
}

async fn config_set(State(st): St, Json(body): Json<ConfigIn>) -> Response {
    blocking(move || {
        let mut c = body.config;
        c.provider.api_key = None;
        if !c.provider.model.is_empty() {
            c.provider.validate()?;
        }
        st.engine.db.set_setting("rt_config", &serde_json::to_string(&c)?)?;
        if body.clear_api_key {
            st.engine.db.set_setting("rt_api_key", "")?;
        } else if let Some(k) = body.api_key.filter(|k| !k.is_empty()) {
            st.engine.db.set_setting("rt_api_key", &k)?;
        }
        Ok(json!({"saved": true}))
    })
    .await
}

async fn provider_test(State(st): St) -> Response {
    blocking(move || {
        let cfg = load_config(&st.engine.db);
        let mut probe = cfg.provider.clone();
        if probe.model.is_empty() {
            probe.model = "(none)".into();
        }
        let p = OpenAiCompatProvider::new(probe)?;
        let models = p.list_models()?;
        Ok(json!({"reachable": true, "models": models, "selected": p.info(),
            "note": "Reachable only means the server answered. A model is not called supported until a decision proposal and tool execution have been tested with it (see the capability report)."}))
    })
    .await
}

async fn sessions_list(State(st): St) -> Response {
    blocking(move || {
        let conn = st.engine.db.conn();
        let rows = store::rows(&conn, "SELECT s.id, s.status, s.status_reason, s.steps_used, s.tool_calls_used, s.created_at, s.updated_at, s.provider_config, s.parent_session_id, t.title, t.working_dir,
            (SELECT COUNT(*) FROM rt_actions a WHERE a.session_id=s.id) AS actions, (SELECT COUNT(*) FROM rt_findings f WHERE f.session_id=s.id) AS findings
            FROM rt_sessions s JOIN rt_tasks t ON t.id=s.task_id ORDER BY s.created_at DESC LIMIT 200", &[])?;
        Ok(json!({"sessions": rows, "origin": "agenttracker_runtime"}))
    })
    .await
}

async fn session_get(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || {
        let conn = st.engine.db.conn();
        let s = store::row(&conn, "SELECT s.*, t.title, t.instruction, t.protected_paths FROM rt_sessions s JOIN rt_tasks t ON t.id=s.task_id WHERE s.id=?1", &[&id])?.ok_or_else(|| RtError::NotFound(id.clone()))?;
        let decisions = store::rows(&conn, "SELECT d.id, d.step, d.kind, d.tool, d.args, d.approval_required, d.proposal, d.revises_decision_id, d.created_at, a.id AS action_id,
            (SELECT state FROM rt_action_states x WHERE x.action_id=a.id ORDER BY seq DESC LIMIT 1) AS state, (SELECT COUNT(*) FROM rt_approvals p WHERE p.decision_id=d.id) AS approved
            FROM rt_decisions d LEFT JOIN rt_actions a ON a.decision_id=d.id WHERE d.session_id=?1 ORDER BY d.created_at", &[&id])?;
        let attempts = store::rows(&conn, "SELECT id, step, attempt, valid, errors, raw_redacted, created_at FROM rt_proposal_attempts WHERE session_id=?1 ORDER BY created_at", &[&id])?;
        let findings = store::rows(&conn, "SELECT * FROM rt_findings WHERE session_id=?1 ORDER BY created_at", &[&id])?;
        let requests = store::rows(&conn, "SELECT id, step, attempt, purpose, provider, model, model_digest, status, error, structured_output_mode, started_at, finished_at, json_extract(payload_redacted, '$.prompt_chars') AS prompt_chars, json_extract(settings, '$.max_tokens') AS max_tokens, response_redacted FROM rt_model_requests WHERE session_id=?1 ORDER BY started_at", &[&id])?;
        // Release the database lock before load_config, which takes the same (non-reentrant) lock;
        // holding it here deadlocked every later request and froze the whole app.
        drop(conn);
        let model_context_tokens = load_config(&st.engine.db).provider.model_context_tokens;
        Ok(json!({"session": s, "decisions": decisions, "attempts": attempts, "findings": findings, "requests": requests, "model_context_tokens": model_context_tokens}))
    })
    .await
}

#[derive(Deserialize)]
struct TaskIn {
    title: Option<String>,
    instruction: String,
    working_dir: String,
    #[serde(default)]
    protected_paths: Vec<String>,
    #[serde(default)]
    documents: Vec<crate::runtime::DocSpec>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

async fn task_start(State(st): St, Json(t): Json<TaskIn>) -> Response {
    blocking(move || {
        let rt = live_runtime(&st)?;
        let spec = TaskSpec {
            title: t.title.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| t.instruction.chars().take(60).collect()),
            instruction: t.instruction,
            working_dir: PathBuf::from(t.working_dir),
            protected_paths: t.protected_paths.into_iter().filter(|p| !p.trim().is_empty()).collect(),
            documents: t.documents,
            idempotency_key: t.idempotency_key,
            parent_session_id: None,
        };
        let out = rt.start_task(&spec)?;
        let status = if out.existing { rt.status(&out.session_id)? } else { rt.run(&out.session_id)? };
        Ok(json!({"start": out, "status": status}))
    })
    .await
}

/// The live model a session was started with, so Resume and Approve keep using it (a retried run
/// keeps its chosen model). Fixture/simulated sessions and older records fall back to the saved model.
fn session_model(db: &Db, session_id: &str) -> Option<String> {
    let conn = db.conn();
    let pc: String = conn.query_row("SELECT provider_config FROM rt_sessions WHERE id=?1", [session_id], |r| r.get(0)).ok()?;
    drop(conn);
    let v: Value = serde_json::from_str(&pc).ok()?;
    if v["is_fixture"].as_bool().unwrap_or(false) {
        return None;
    }
    v["model"].as_str().map(str::trim).filter(|m| !m.is_empty()).map(String::from)
}

fn runtime_for_session(st: &AppState, session_id: &str) -> crate::Result<Runtime> {
    let model = session_model(&st.engine.db, session_id);
    live_runtime_with_model(st, model.as_deref())
}

async fn provider_models(State(st): St) -> Response {
    blocking(move || {
        let mut probe = load_config(&st.engine.db).provider;
        if probe.model.trim().is_empty() {
            probe.model = "(none)".into();
        }
        let models: Vec<String> = OpenAiCompatProvider::new(probe)?.list_models()?.into_iter().filter(|m| !m.contains("embed")).collect();
        Ok(json!({"models": models}))
    })
    .await
}

#[derive(Deserialize, Default)]
struct RetryIn {
    #[serde(default)]
    model: String,
}

/// "Retry with this model": starts a NEW session with the same request (fresh folder for the
/// built-in demos) and runs it with `model` for that run only. The old session is not modified.
async fn session_retry(State(st): St, UrlPath(id): UrlPath<String>, body: Option<Json<RetryIn>>) -> Response {
    let model = body.map(|b| b.0.model).unwrap_or_default().trim().to_string();
    blocking(move || {
        if model.is_empty() {
            return Err(RtError::Invalid("Pick a model to try first.".into()));
        }
        let spec = crate::runtime::retry_spec(&st.engine.db, &id, Some(&st.agenttracker_home.join("runtime-demos")))?;
        let rt = live_runtime_with_model(&st, Some(&model))?;
        let out = rt.start_task(&spec)?;
        // The new run exists from here on; report a run problem alongside its id instead of losing it.
        let (status, run_error) = match rt.run(&out.session_id) {
            Ok(s) => (serde_json::to_value(s)?, Value::Null),
            Err(e) => (Value::Null, json!(e.to_string())),
        };
        Ok(json!({"start": out, "status": status, "run_error": run_error, "model": model, "retried_from": id}))
    })
    .await
}

async fn session_resume(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || {
        let rt = runtime_for_session(&st, &id)?;
        st.engine.db.conn().execute("UPDATE rt_sessions SET status='running' WHERE id=?1 AND status='paused'", [&id])?;
        Ok(json!({"status": rt.run(&id)?}))
    })
    .await
}

async fn decision_approve(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || {
        let sid: String = st.engine.db.conn().query_row("SELECT session_id FROM rt_decisions WHERE id=?1", [&id], |r| r.get(0))?;
        let rt = runtime_for_session(&st, &sid)?;
        let approval = rt.approve(&id, "local user (Houndbit UI)")?;
        Ok(json!({"approval_id": approval, "status": rt.run(&sid)?}))
    })
    .await
}

async fn action_why(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || why::bundle_for_action(&st.engine.db, &id)).await
}

async fn decision_why(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || why::bundle_for_decision(&st.engine.db, &id)).await
}

async fn decision_analyze(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || {
        let rt = live_runtime(&st)?;
        Ok(json!({"finding_id": checks::analyze_with_model(&st.engine.db, rt.provider.as_ref(), &id)?}))
    })
    .await
}

async fn investigation_start(State(st): St, Json(spec): Json<InvestigationSpec>) -> Response {
    blocking(move || {
        let rt = live_runtime(&st)?;
        let v = rt.vault.clone().or_else(|| ReplayVault::open(&replay_vault::vault_dir(&st.agenttracker_home)).ok().map(Arc::new));
        let id = investigate::run(&st.engine.db, rt.provider.as_ref(), v.as_deref(), &spec)?;
        investigation_json(&st.engine.db, &id)
    })
    .await
}

pub fn investigation_json(db: &Db, id: &str) -> crate::Result<Value> {
    let conn = db.conn();
    let inv = store::row(&conn, "SELECT * FROM rt_investigations WHERE id=?1", &[&id])?.ok_or_else(|| RtError::NotFound(id.into()))?;
    let trials = store::rows(&conn, "SELECT * FROM rt_replay_trials WHERE investigation_id=?1 ORDER BY arm, trial_no", &[&id])?;
    Ok(json!({"investigation": inv, "trials": trials, "label": "Experimental: behavioural sensitivity under the tested conditions, not proof of cause."}))
}

async fn investigation_get(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || investigation_json(&st.engine.db, &id)).await
}

#[derive(Deserialize, Default)]
struct DemoIn {
    #[serde(default)]
    mode: Option<String>,
}

async fn demo_run(State(st): St, UrlPath(which): UrlPath<String>, body: Option<Json<DemoIn>>) -> Response {
    let mode = body.and_then(|b| b.0.mode).unwrap_or_else(|| "fixture".into());
    blocking(move || {
        let which = match which.as_str() {
            "a" | "A" => Demo::A,
            "b" | "B" => Demo::B,
            _ => return Err(RtError::Invalid("demo must be a or b".into())),
        };
        let base = st.agenttracker_home.join("runtime-demos");
        let (dir, spec) = demo::prepare(&base, which)?;
        let rt = if mode == "live" {
            live_runtime(&st)?
        } else {
            let cfg = load_config(&st.engine.db);
            Runtime::new(st.engine.db.clone(), Arc::new(demo::fixture_provider(which)), cfg, None)
        };
        let out = rt.start_task(&spec)?;
        let status = rt.run(&out.session_id)?;
        Ok(json!({"mode": mode, "fixture": mode != "live", "folder": dir.to_string_lossy(), "start": out, "status": status}))
    })
    .await
}

async fn replay_delete(State(st): St) -> Response {
    blocking(move || Ok(json!({"deleted": replay_vault::delete_all(&st.engine.db.conn())?}))).await
}

async fn recover(State(st): St) -> Response {
    blocking(move || Ok(json!({"recovered": Runtime::recover_unknown(&st.engine.db)?}))).await
}

// ---- chat recorder (relay) ----------------------------------------------------------------------

fn relay_state(db: &Db) -> crate::Result<Value> {
    let cfg = relay::load_config(db);
    let st = relay::status();
    let port = st["port"].as_u64().unwrap_or(cfg.port as u64);
    let conn = db.conn();
    let (calls, convs): (i64, i64) = conn.query_row("SELECT COUNT(*), COUNT(DISTINCT conversation_id) FROM relay_calls", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(json!({"config": cfg, "status": st, "address": format!("http://127.0.0.1:{port}/v1"), "ollama_address": format!("http://127.0.0.1:{port}"),
        "calls": calls, "conversations": convs}))
}

async fn relay_get(State(st): St) -> Response {
    blocking(move || relay_state(&st.engine.db)).await
}

async fn relay_set(State(st): St, Json(cfg): Json<RelayConfig>) -> Response {
    let mut cfg = cfg;
    cfg.upstream = relay::upstream_root(&cfg.upstream);
    if let Err(e) = relay::validate(&cfg) {
        return rt_err(e);
    }
    if let Err(e) = relay::save_config(&st.engine.db, &cfg) {
        return rt_err(e);
    }
    if cfg.enabled {
        if let Err(e) = relay::start(st.engine.db.clone(), cfg).await {
            return rt_err(e);
        }
    } else {
        relay::stop();
    }
    blocking(move || relay_state(&st.engine.db)).await
}

/// Asks the model server for its model list THROUGH the recorder, so a pass means the whole path
/// (recorder -> model server) answers. Model-list calls are not recorded.
async fn relay_test(State(_st): St) -> Response {
    blocking(move || {
        let Some(port) = relay::running_port() else {
            return Err(RtError::Invalid("The chat recorder is off. Turn it on first.".into()));
        };
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
        let mut tried = Vec::new();
        for path in ["/v1/models", "/api/tags"] {
            match agent.get(&format!("http://127.0.0.1:{port}{path}")).call() {
                Ok(r) => {
                    let v: Value = r.into_json().unwrap_or(Value::Null);
                    let models: Vec<String> = v["data"].as_array().or_else(|| v["models"].as_array()).into_iter().flatten()
                        .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()).map(String::from)).collect();
                    return Ok(json!({"ok": true, "models": models, "detail": format!("Your model server answered through the recorder ({path}).")}));
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    tried.push(format!("{path}: HTTP {code} {}", body.chars().take(200).collect::<String>()));
                }
                Err(e) => tried.push(format!("{path}: {e}")),
            }
        }
        Ok(json!({"ok": false, "models": [], "detail": tracker_common::redaction::redact_text(&tried.join(" | "))}))
    })
    .await
}

async fn relay_conversations(State(st): St) -> Response {
    blocking(move || Ok(json!({"conversations": relay::list_conversations(&st.engine.db)?}))).await
}

async fn relay_conversation(State(st): St, UrlPath(id): UrlPath<String>) -> Response {
    blocking(move || {
        let conn = st.engine.db.conn();
        let mut calls = store::rows(&conn, "SELECT * FROM relay_calls WHERE conversation_id=?1 ORDER BY started_at", &[&id])?;
        if calls.is_empty() {
            return Err(RtError::NotFound(id));
        }
        calls.iter_mut().for_each(relay::annotate_call);
        let typed = calls.iter().filter(|c| c["typed_tool_call"].as_bool() == Some(true)).count();
        Ok(json!({"conversation_id": id, "calls": calls, "typed_tool_call_count": typed}))
    })
    .await
}

#[derive(Deserialize, Default)]
struct RelayDeleteIn {
    #[serde(default)]
    conversation_id: Option<String>,
}

async fn relay_delete(State(st): St, body: Option<Json<RelayDeleteIn>>) -> Response {
    let conv = body.and_then(|b| b.0.conversation_id).filter(|s| !s.is_empty());
    blocking(move || {
        let conn = st.engine.db.conn();
        let n = match &conv {
            Some(c) => conn.execute("DELETE FROM relay_calls WHERE conversation_id=?1", [c])?,
            None => conn.execute("DELETE FROM relay_calls", [])?,
        };
        Ok(json!({"deleted": n}))
    })
    .await
}
