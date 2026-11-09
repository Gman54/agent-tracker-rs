//! Chat recorder: a small local relay between a chat app and the user's own model server.
//!
//! `chat app -> 127.0.0.1:1235 (this relay) -> LM Studio / Ollama`
//!
//! Every request is passed through unchanged and the reply is streamed back byte for byte. Chat
//! calls (OpenAI-style `/v1/chat/completions`, `/v1/completions`, `/v1/responses`, Ollama
//! `/api/chat`, `/api/generate`) are recorded, redacted, in `relay_calls`: the messages sent, the
//! reply, tool calls, and the model's thinking text when the server sends it. Other paths
//! (`/v1/models`, ...) are forwarded but not recorded.
//!
//! The relay binds to 127.0.0.1 only, refuses requests addressed to other host names (DNS
//! rebinding guard) and only forwards to a model server on this computer. Recorded rows can be
//! deleted by the user; the hash chain keeps only their hashes, not their content.
use crate::ids::new_id;
use crate::store;
use crate::{now_ms, sha256_hex, Result, RtError};
use agenttracker_core::db::Db;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 1235;
pub const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:1234";
const CALL_PREFIX: &str = "rly";
/// Request bodies larger than this are refused (images in chats can be big).
const MAX_REQUEST: usize = 64 << 20;
/// Reply bytes kept for recording; the chat app still receives everything.
const KEEP_LIMIT: usize = 16 << 20;
const TEXT_CAP: usize = 100_000;
const DEEP_STRING_CAP: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_port")]
    pub port: u16,
    /// Where the model server listens, e.g. http://127.0.0.1:1234 (LM Studio) or :11434 (Ollama).
    #[serde(default)]
    pub upstream: String,
}

fn d_port() -> u16 {
    DEFAULT_PORT
}

impl Default for RelayConfig {
    fn default() -> Self {
        RelayConfig { enabled: false, port: DEFAULT_PORT, upstream: String::new() }
    }
}

/// Saved relay settings. An empty model-server address falls back to the Agent Runtime's model
/// server when one was saved, otherwise LM Studio's default.
pub fn load_config(db: &Db) -> RelayConfig {
    let mut c: RelayConfig = db.get_setting("relay_config").ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if c.upstream.trim().is_empty() {
        c.upstream = db
            .get_setting("rt_config")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["provider"]["endpoint"].as_str().map(upstream_root))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string());
    }
    c
}

pub fn save_config(db: &Db, c: &RelayConfig) -> Result<()> {
    db.set_setting("relay_config", &serde_json::to_string(c)?)?;
    Ok(())
}

/// `http://127.0.0.1:1234/v1/` -> `http://127.0.0.1:1234` (the chat app's path is appended as-is).
pub fn upstream_root(u: &str) -> String {
    let b = u.trim().trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).trim_end_matches('/').to_string()
}

fn host_port(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("http://")?;
    let hp = rest.split('/').next().unwrap_or("");
    if let Some(r) = hp.strip_prefix('[') {
        let (h, p) = r.split_once(']')?;
        let port = p.strip_prefix(':').map(|x| x.parse().ok()).unwrap_or(Some(80))?;
        return Some((h.to_string(), port));
    }
    match hp.split_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((hp.to_string(), 80)),
    }
}

fn is_loopback(h: &str) -> bool {
    matches!(h.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1")
}

/// Port 0 means "any free port" (used by tests).
pub fn validate(c: &RelayConfig) -> Result<()> {
    if c.port != 0 && c.port < 1024 {
        return Err(RtError::Invalid("Pick a port number from 1024 to 65535. 1235 is a good choice.".into()));
    }
    let up = upstream_root(&c.upstream);
    let Some((host, port)) = host_port(&up) else {
        return Err(RtError::Invalid("The model server address should look like http://127.0.0.1:1234 (LM Studio) or http://127.0.0.1:11434 (Ollama).".into()));
    };
    if !is_loopback(&host) {
        return Err(RtError::Invalid("The recorder only forwards to a model server on this computer (127.0.0.1 or localhost).".into()));
    }
    if c.port != 0 && port == c.port {
        return Err(RtError::Invalid("The recorder's port and the model server's port are the same. Use a different port for the recorder (for example 1235).".into()));
    }
    Ok(())
}

struct Running {
    port: u16,
    upstream: String,
    stop: tokio::sync::oneshot::Sender<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `{running, port, upstream, error}` for the UI.
pub fn status() -> Value {
    let r = lock(&RUNNING);
    let err = lock(&LAST_ERROR).clone();
    match r.as_ref() {
        Some(x) => json!({"running": true, "port": x.port, "upstream": x.upstream, "error": null}),
        None => json!({"running": false, "port": null, "upstream": null, "error": err}),
    }
}

pub fn running_port() -> Option<u16> {
    lock(&RUNNING).as_ref().map(|r| r.port)
}

/// Stops the relay if it is running.
pub fn stop() {
    if let Some(r) = lock(&RUNNING).take() {
        let _ = r.stop.send(());
    }
}

/// Starts (or restarts) the relay. Must be called inside a tokio runtime. Returns the bound port.
pub async fn start(db: Db, cfg: RelayConfig) -> Result<u16> {
    validate(&cfg)?;
    stop();
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::AddrInUse {
                format!("Port {} is already used by another program. Pick a different port for the recorder.", cfg.port)
            } else {
                format!("Could not open port {}: {e}", cfg.port)
            };
            *lock(&LAST_ERROR) = Some(msg.clone());
            return Err(RtError::Refused(msg));
        }
    };
    let port = listener.local_addr()?.port();
    let upstream = upstream_root(&cfg.upstream);
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout_read(Duration::from_secs(900)).build();
    let st = Arc::new(Relay { db, upstream: upstream.clone(), agent });
    let app = Router::new().fallback(handle).with_state(st);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    *lock(&LAST_ERROR) = None;
    *lock(&RUNNING) = Some(Running { port, upstream, stop: tx });
    Ok(port)
}

/// Starts the relay at app launch when the user turned it on. Call inside a tokio runtime.
pub fn autostart(db: Db) {
    let cfg = load_config(&db);
    if !cfg.enabled {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = start(db, cfg).await {
            eprintln!("chat recorder did not start: {e}");
        }
    });
}

struct Relay {
    db: Db,
    upstream: String,
    agent: ureq::Agent,
}

fn err_json(status: u16, msg: &str) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), Json(json!({"error": {"message": msg, "type": "agenttracker_relay"}}))).into_response()
}

/// Which chat API a POST path belongs to; `None` means "forward without recording".
pub fn detect_api(path: &str) -> Option<&'static str> {
    let p = path.trim_end_matches('/');
    if p.ends_with("/chat/completions") {
        Some("chat_completions")
    } else if p.ends_with("/completions") {
        Some("completions")
    } else if p.ends_with("/responses") {
        Some("responses")
    } else if p == "/api/chat" {
        Some("ollama_chat")
    } else if p == "/api/generate" {
        Some("ollama_generate")
    } else {
        None
    }
}

type Head = std::result::Result<(u16, Option<String>), String>;
type Chunk = std::result::Result<Bytes, std::io::Error>;

async fn handle(State(r): State<Arc<Relay>>, req: Request) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
    let host_name = if host.starts_with('[') { host.trim_start_matches('[').split(']').next().unwrap_or("").to_string() } else { host.split(':').next().unwrap_or("").to_string() };
    if !is_loopback(&host_name) {
        // Read (and throw away) the request body before refusing. Replying while the body is still
        // on its way makes the server close a socket that then receives data; Windows answers that
        // with a reset and the client can lose the 403 we already sent (it saw a connection error
        // instead). Bounded in size and time so a refused client can't hold the connection open.
        let _ = tokio::time::timeout(Duration::from_secs(5), axum::body::to_bytes(req.into_body(), 1 << 20)).await;
        let mut resp = err_json(403, "The Houndbit chat recorder only accepts requests addressed to 127.0.0.1 or localhost.");
        resp.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
        return resp;
    }
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let mut headers: Vec<(String, String)> = Vec::new();
    for name in ["content-type", "authorization", "accept", "user-agent"] {
        if let Some(v) = req.headers().get(name).and_then(|v| v.to_str().ok()) {
            headers.push((name.to_string(), v.to_string()));
        }
    }
    let client = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(String::from);
    let body = match axum::body::to_bytes(req.into_body(), MAX_REQUEST).await {
        Ok(b) => b,
        Err(_) => return err_json(413, "The request is too large for the recorder (64 MB limit)."),
    };
    let api = if method == "POST" { detect_api(&path) } else { None };
    let (head_tx, head_rx) = tokio::sync::oneshot::channel::<Head>();
    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Chunk>(64);
    let job = Forward { url: format!("{}{}", r.upstream, pq), method, headers, body, api, path, client };
    let r2 = r.clone();
    tokio::task::spawn_blocking(move || forward(&r2, job, head_tx, chunk_tx));
    match head_rx.await {
        Ok(Ok((status, ct))) => {
            let stream = futures_util::stream::unfold(chunk_rx, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) });
            let mut resp = Response::new(Body::from_stream(stream));
            *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            if let Some(v) = ct.and_then(|c| HeaderValue::from_str(&c).ok()) {
                resp.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            resp
        }
        Ok(Err(msg)) => err_json(502, &msg),
        Err(_) => err_json(502, "The recorder's worker stopped unexpectedly."),
    }
}

struct Forward {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Bytes,
    api: Option<&'static str>,
    path: String,
    client: Option<String>,
}

fn forward(r: &Relay, job: Forward, head_tx: tokio::sync::oneshot::Sender<Head>, chunk_tx: tokio::sync::mpsc::Sender<Chunk>) {
    let started = now_ms();
    let mut rq = r.agent.request(&job.method, &job.url);
    for (k, v) in &job.headers {
        rq = rq.set(k, v);
    }
    let res = if job.body.is_empty() && job.method != "POST" { rq.call() } else { rq.send_bytes(&job.body) };
    let resp = match res {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(ureq::Error::Transport(t)) => {
            let msg = format!("Houndbit could not reach your model server at {}. Check that LM Studio's server (or Ollama) is running. ({t})", r.upstream);
            let _ = head_tx.send(Err(msg.clone()));
            if let Some(api) = job.api {
                let call = Call { api, path: &job.path, client: job.client.clone(), request: &job.body, response: &[], content_type: None, status: None, error: Some(msg), started };
                if let Err(e) = record(&r.db, &call) {
                    eprintln!("chat recorder could not save a call: {e}");
                }
            }
            return;
        }
    };
    let status = resp.status();
    let ct = resp.header("content-type").map(String::from);
    let _ = head_tx.send(Ok((status, ct.clone())));
    let mut reader = resp.into_reader();
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut problem: Option<String> = None;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if job.api.is_some() && kept.len() < KEEP_LIMIT {
                    kept.extend_from_slice(&buf[..n]);
                }
                if chunk_tx.blocking_send(Ok(Bytes::copy_from_slice(&buf[..n]))).is_err() {
                    problem = Some("The chat app stopped the reply early (Stop button, or the app closed the connection).".into());
                    break;
                }
            }
            Err(e) => {
                problem = Some(format!("The model server's reply was cut off: {e}"));
                let _ = chunk_tx.blocking_send(Err(e));
                break;
            }
        }
    }
    drop(chunk_tx);
    if let Some(api) = job.api {
        let call = Call { api, path: &job.path, client: job.client, request: &job.body, response: &kept, content_type: ct, status: Some(status), error: problem, started };
        if let Err(e) = record(&r.db, &call) {
            eprintln!("chat recorder could not save a call: {e}");
        }
    }
}

/// One finished call, as seen by the relay.
pub struct Call<'a> {
    pub api: &'static str,
    pub path: &'a str,
    pub client: Option<String>,
    pub request: &'a [u8],
    pub response: &'a [u8],
    pub content_type: Option<String>,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub started: i64,
}

/// What the reply contained, assembled from a plain JSON reply or a stream.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    pub reply: String,
    pub reasoning: String,
    pub reasoning_source: Option<String>,
    /// (id, name, arguments as text)
    pub tool_calls: Vec<(Option<String>, String, String)>,
    pub finish_reason: Option<String>,
    pub usage: Value,
    pub streamed: bool,
    pub upstream_error: Option<String>,
}

impl Parsed {
    fn add_reasoning(&mut self, s: &str, source: &str) {
        if s.is_empty() {
            return;
        }
        self.reasoning.push_str(s);
        if self.reasoning_source.is_none() {
            self.reasoning_source = Some(source.into());
        }
    }

    fn merge_tool(&mut self, idx: usize, t: &Value) {
        while self.tool_calls.len() <= idx {
            self.tool_calls.push((None, String::new(), String::new()));
        }
        let e = &mut self.tool_calls[idx];
        if let Some(id) = t["id"].as_str() {
            e.0 = Some(id.into());
        }
        let f = &t["function"];
        if let Some(n) = f["name"].as_str().filter(|n| !n.is_empty()) {
            e.1 = n.into();
        }
        match &f["arguments"] {
            Value::String(s) => e.2.push_str(s),
            Value::Null => {}
            other => e.2 = other.to_string(),
        }
    }

    fn absorb_response_obj(&mut self, r: &Value, with_text: bool) {
        if r["usage"].is_object() {
            self.usage = r["usage"].clone();
        }
        if let Some(s) = r["status"].as_str() {
            self.finish_reason = Some(s.into());
        }
        for item in r["output"].as_array().into_iter().flatten() {
            match item["type"].as_str().unwrap_or("") {
                "message" if with_text => {
                    for part in item["content"].as_array().into_iter().flatten() {
                        if let Some(t) = part["text"].as_str() {
                            self.reply.push_str(t);
                        }
                    }
                }
                "reasoning" if with_text || self.reasoning.is_empty() => {
                    for key in ["summary", "content"] {
                        for part in item[key].as_array().into_iter().flatten() {
                            if let Some(t) = part["text"].as_str() {
                                self.add_reasoning(t, "reasoning_items");
                            }
                        }
                    }
                }
                "function_call" => {
                    let args = match &item["arguments"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    self.tool_calls.push((item["call_id"].as_str().map(String::from), item["name"].as_str().unwrap_or("").into(), args));
                }
                _ => {}
            }
        }
    }

    fn absorb(&mut self, api: &str, v: &Value) {
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            self.upstream_error = Some(e["message"].as_str().map(String::from).unwrap_or_else(|| e.to_string()));
        }
        match api {
            "chat_completions" | "completions" => {
                if let Some(c) = v["choices"].get(0) {
                    let m = if c.get("delta").is_some() { &c["delta"] } else { &c["message"] };
                    if let Some(s) = m["content"].as_str() {
                        self.reply.push_str(s);
                    }
                    if let Some(s) = c["text"].as_str() {
                        self.reply.push_str(s);
                    }
                    for key in ["reasoning_content", "reasoning"] {
                        if let Some(s) = m[key].as_str() {
                            self.add_reasoning(s, key);
                        }
                    }
                    for (i, t) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
                        let idx = t["index"].as_u64().map(|x| x as usize).unwrap_or(i);
                        self.merge_tool(idx, t);
                    }
                    if let Some(f) = c["finish_reason"].as_str() {
                        self.finish_reason = Some(f.into());
                    }
                }
                if v["usage"].is_object() {
                    self.usage = v["usage"].clone();
                }
            }
            "responses" => match v["type"].as_str() {
                Some("response.output_text.delta") => self.reply.push_str(v["delta"].as_str().unwrap_or("")),
                Some("response.reasoning_text.delta") | Some("response.reasoning_summary_text.delta") => {
                    let d = v["delta"].as_str().unwrap_or("").to_string();
                    self.add_reasoning(&d, "reasoning_items");
                }
                Some("response.completed") | Some("response.incomplete") | Some("response.failed") => {
                    let with_text = self.reply.is_empty();
                    self.absorb_response_obj(&v["response"], with_text);
                }
                Some(_) => {}
                None => self.absorb_response_obj(v, true),
            },
            "ollama_chat" | "ollama_generate" => {
                let m = &v["message"];
                if let Some(s) = m["content"].as_str().or_else(|| v["response"].as_str()) {
                    self.reply.push_str(s);
                }
                if let Some(s) = m["thinking"].as_str().or_else(|| v["thinking"].as_str()) {
                    self.add_reasoning(s, "thinking");
                }
                for t in m["tool_calls"].as_array().into_iter().flatten() {
                    let idx = self.tool_calls.len();
                    self.merge_tool(idx, t);
                }
                if v["done"].as_bool() == Some(true) {
                    self.finish_reason = v["done_reason"].as_str().map(String::from).or(Some("stop".into()));
                    if v.get("eval_count").is_some() || v.get("prompt_eval_count").is_some() {
                        self.usage = json!({"prompt_tokens": v["prompt_eval_count"], "completion_tokens": v["eval_count"]});
                    }
                }
            }
            _ => {}
        }
    }

    /// Models that think out loud inside `<think>...</think>` (or whose template already opened the
    /// tag, leaving only `</think>`): move that text out of the reply into the thinking field.
    fn split_think_tags(&mut self) {
        if !self.reasoning.trim().is_empty() {
            return;
        }
        let (thought, rest) = if let Some(start) = self.reply.find("<think>") {
            let after = &self.reply[start + 7..];
            match after.find("</think>") {
                Some(e) => (after[..e].to_string(), format!("{}{}", &self.reply[..start], &after[e + 8..])),
                None => (after.to_string(), self.reply[..start].to_string()),
            }
        } else if let Some(e) = self.reply.find("</think>") {
            (self.reply[..e].to_string(), self.reply[e + 8..].to_string())
        } else {
            return;
        };
        self.reasoning = thought.trim().to_string();
        self.reply = rest.trim().to_string();
        if !self.reasoning.is_empty() {
            self.reasoning_source = Some("think_tags".into());
        }
    }
}

/// Assembles the reply from a plain JSON body, an SSE stream (`data: {...}`) or Ollama NDJSON.
pub fn parse_response(api: &str, content_type: Option<&str>, body: &[u8]) -> Parsed {
    let text = String::from_utf8_lossy(body);
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let trimmed = text.trim_start();
    let mut p = Parsed::default();
    if ct.contains("text/event-stream") || trimmed.starts_with("data:") || trimmed.starts_with("event:") {
        p.streamed = true;
        for line in text.lines() {
            if let Some(d) = line.trim().strip_prefix("data:") {
                let d = d.trim();
                if d.is_empty() || d == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(d) {
                    p.absorb(api, &v);
                }
            }
        }
    } else if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        p.streamed = ct.contains("ndjson");
        p.absorb(api, &v);
    } else {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                p.streamed = true;
                p.absorb(api, &v);
            }
        }
    }
    p.split_think_tags();
    p
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.as_str().or_else(|| p["text"].as_str())).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// (role, text) for each message the chat app sent.
pub fn request_messages(api: &str, req: &Value) -> Vec<(String, String)> {
    let role_of = |m: &Value, dflt: &str| m["role"].as_str().unwrap_or(dflt).to_string();
    match api {
        "chat_completions" | "ollama_chat" => req["messages"].as_array().into_iter().flatten().map(|m| (role_of(m, "user"), text_of(&m["content"]))).collect(),
        "responses" => {
            let mut out = Vec::new();
            if let Some(s) = req["instructions"].as_str() {
                out.push(("system".to_string(), s.to_string()));
            }
            match &req["input"] {
                Value::String(s) => out.push(("user".into(), s.clone())),
                Value::Array(items) => {
                    for it in items {
                        let role = role_of(it, "tool");
                        let text = if it.get("content").is_some() { text_of(&it["content"]) } else { it["output"].as_str().unwrap_or("").to_string() };
                        out.push((role, text));
                    }
                }
                _ => {}
            }
            out
        }
        _ => {
            let mut out = Vec::new();
            if let Some(s) = req["system"].as_str() {
                out.push(("system".to_string(), s.to_string()));
            }
            let prompt = text_of(&req["prompt"]);
            if !prompt.is_empty() {
                out.push(("user".into(), prompt));
            }
            out
        }
    }
}

fn cap(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    format!("{}\n[... {} more characters not kept]", s.chars().take(n).collect::<String>(), count - n)
}

fn cap_deep(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(cap(s, DEEP_STRING_CAP)),
        Value::Array(a) => Value::Array(a.iter().map(cap_deep).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), cap_deep(x))).collect()),
        other => other.clone(),
    }
}

/// Same first system message + first user message = same conversation (a best guess: chat apps
/// resend the whole conversation on every turn).
pub fn conversation_id(msgs: &[(String, String)]) -> String {
    let first = |roles: &[&str]| msgs.iter().find(|(r, _)| roles.contains(&r.as_str())).map(|(_, t)| t.as_str()).unwrap_or("");
    let key = format!("{}\u{1f}{}", first(&["system", "developer"]), first(&["user"]));
    format!("conv_{}", &sha256_hex(key.as_bytes())[..24])
}

/// Builds the `relay_calls` row for one call (redacted).
pub fn build_row(c: &Call) -> Value {
    use tracker_common::redaction::{redact_deep, redact_text};
    let req: Value = serde_json::from_slice(c.request).unwrap_or(Value::Null);
    let msgs = request_messages(c.api, &req);
    let first_user = msgs.iter().find(|(r, _)| r == "user").map(|(_, t)| t.clone()).unwrap_or_default();
    let last_user = msgs.iter().rev().find(|(r, _)| r == "user").map(|(_, t)| t.clone()).unwrap_or_default();
    let one_line: String = first_user.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if one_line.is_empty() { "(no user message)".to_string() } else { redact_text(&if one_line.chars().count() > 80 { format!("{}...", one_line.chars().take(80).collect::<String>()) } else { one_line.clone() }) };
    let p = parse_response(c.api, c.content_type.as_deref(), c.response);
    let tool_calls = if p.tool_calls.is_empty() {
        Value::Null
    } else {
        redact_deep(&Value::Array(p.tool_calls.iter().map(|(id, n, a)| json!({"id": id, "name": n, "arguments": cap(a, DEEP_STRING_CAP)})).collect()))
    };
    let mut error = c.error.clone().or(p.upstream_error.clone());
    if error.is_none() && c.status.map(|s| s >= 400).unwrap_or(false) {
        error = Some(cap(&String::from_utf8_lossy(c.response), 300));
    }
    let request_redacted = if req.is_null() { json!(format!("(not JSON, {} bytes)", c.request.len())) } else { cap_deep(&redact_deep(&req)) };
    let opt = |s: &str| if s.trim().is_empty() { Value::Null } else { json!(redact_text(&cap(s, TEXT_CAP))) };
    json!({
        "id": new_id(CALL_PREFIX),
        "conversation_id": conversation_id(&msgs),
        "title": title,
        "api": c.api,
        "path": c.path,
        "client": c.client.as_deref().map(|s| cap(s, 200)),
        "model": req["model"].as_str(),
        "streamed": p.streamed,
        "message_count": msgs.len(),
        "last_user_redacted": opt(&last_user),
        "request_redacted": request_redacted,
        "reply_redacted": opt(&p.reply),
        "reasoning_redacted": opt(&p.reasoning),
        "reasoning_source": if p.reasoning.trim().is_empty() { None } else { p.reasoning_source.clone() },
        "tool_calls": tool_calls,
        "finish_reason": p.finish_reason,
        "usage": if p.usage.is_object() { p.usage.clone() } else { Value::Null },
        "http_status": c.status,
        "error": error.map(|e| redact_text(&e)),
        "request_sha256": sha256_hex(c.request),
        "response_sha256": if c.status.is_some() { Some(sha256_hex(c.response)) } else { None },
        "started_at": c.started,
        "finished_at": now_ms(),
    })
}

/// Saves one call. The hash chain gets only hashes, so a deleted recording leaves no content behind.
pub fn record(db: &Db, c: &Call) -> Result<String> {
    let row = build_row(c);
    let id = row["id"].as_str().unwrap_or_default().to_string();
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    store::insert_plain(&tx, "relay_calls", &row)?;
    let ev = json!({"rt_table": "relay_calls", "id": id, "conversation_id": row["conversation_id"], "request_sha256": row["request_sha256"], "response_sha256": row["response_sha256"], "started_at": row["started_at"]});
    tracker_common::hash_chain::ChainStore::append_with(&tx, &id, &ev, now_ms())?;
    tx.commit()?;
    Ok(id)
}
