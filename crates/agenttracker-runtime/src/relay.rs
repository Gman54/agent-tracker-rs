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
/// Bytes of the end of a reply kept to look for the end-of-reply marker after `KEEP_LIMIT`.
const TAIL_KEEP: usize = 64 * 1024;
/// The note `cap()` adds to text it shortened.
const CAP_NOTE: &str = " more characters not kept]";

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
/// Also forgives the common ways people copy the address: no `http://` (`localhost:1234`), and the
/// "listen everywhere" addresses LM Studio / Ollama print (`0.0.0.0`, `[::]`), which mean this
/// computer and are reached through 127.0.0.1.
pub fn upstream_root(u: &str) -> String {
    let t = u.trim();
    let with_scheme = if t.is_empty() || t.contains("://") { t.to_string() } else { format!("http://{t}") };
    let b = with_scheme.trim_end_matches('/');
    let b = b.strip_suffix("/v1").unwrap_or(b).trim_end_matches('/');
    for any in ["http://0.0.0.0", "http://[::]"] {
        if let Some(rest) = b.strip_prefix(any) {
            if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
                return format!("http://127.0.0.1{rest}");
            }
        }
    }
    b.to_string()
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

/// True when `h` names this computer: localhost, any 127.x address, ::1, or an IP address that
/// belongs to one of this computer's own network adapters (LM Studio shows e.g. 192.168.1.20
/// when "serve on local network" is on). Host *names* other than localhost are still refused,
/// which keeps the DNS-rebinding guard.
pub fn is_this_computer(h: &str) -> bool {
    let h = h.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    if h == "localhost" {
        return true;
    }
    let Ok(ip) = h.parse::<std::net::IpAddr>() else { return false };
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    // Binding succeeds only for addresses owned by this machine.
    std::net::UdpSocket::bind((ip, 0)).is_ok()
}

fn is_loopback(h: &str) -> bool {
    is_this_computer(h)
}

/// Port 0 means "any free port" (used by tests).
pub fn validate(c: &RelayConfig) -> Result<()> {
    if c.port != 0 && c.port < 1024 {
        return Err(RtError::Invalid("Pick a port number from 1024 to 65535. 1235 is a good choice.".into()));
    }
    let up = upstream_root(&c.upstream);
    if up.starts_with("https://") {
        return Err(RtError::Invalid(format!("The model server address \"{}\" starts with https://. LM Studio and Ollama use plain http://, for example http://127.0.0.1:1234.", c.upstream.trim())));
    }
    let Some((host, port)) = host_port(&up) else {
        return Err(RtError::Invalid(format!("Houndbit couldn't read the model server address \"{}\". It should look like http://127.0.0.1:1234 (LM Studio) or http://127.0.0.1:11434 (Ollama).", c.upstream.trim())));
    };
    if !is_loopback(&host) {
        return Err(RtError::Invalid(format!("The model server address \"{}\" doesn't point at this computer, so the recorder won't use it. Try http://127.0.0.1:1234 for LM Studio or http://127.0.0.1:11434 for Ollama.", c.upstream.trim())));
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
                let call = Call { api, path: &job.path, client: job.client.clone(), request: &job.body, error: Some(msg), started, finished: now_ms(), ..Default::default() };
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
    // The last bytes received, so the end-of-reply marker is still seen when `kept` is full.
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let (mut first_chunk, mut last_chunk, mut chunks, mut bytes) = (None, None, 0i64, 0i64);
    let mut downstream_closed = false;
    let mut stream_error: Option<String> = None;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let t = now_ms();
                first_chunk.get_or_insert(t);
                last_chunk = Some(t);
                chunks += 1;
                bytes += n as i64;
                if job.api.is_some() {
                    if kept.len() < KEEP_LIMIT {
                        kept.extend_from_slice(&buf[..n]);
                    }
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > TAIL_KEEP {
                        tail.drain(..tail.len() - TAIL_KEEP);
                    }
                }
                if chunk_tx.blocking_send(Ok(Bytes::copy_from_slice(&buf[..n]))).is_err() {
                    // The chat app's side of the connection is gone (Stop button, closed tab, timeout).
                    downstream_closed = true;
                    break;
                }
            }
            Err(e) => {
                stream_error = Some(format!("The connection to the model server broke off: {e}"));
                let _ = chunk_tx.blocking_send(Err(e));
                break;
            }
        }
    }
    // The chat app may have gone while the model server was quiet; nothing was sent after that,
    // so the failed send above never happened. A closed channel here shows it too.
    if chunk_tx.is_closed() {
        downstream_closed = true;
    }
    drop(chunk_tx);
    if let Some(api) = job.api {
        let call = Call {
            api,
            path: &job.path,
            client: job.client,
            request: &job.body,
            response: &kept,
            tail: &tail,
            content_type: ct,
            status: Some(status),
            started,
            finished: now_ms(),
            first_chunk_at: first_chunk,
            last_chunk_at: last_chunk,
            chunk_count: Some(chunks),
            bytes_received: Some(bytes),
            downstream_closed,
            stream_error,
            ..Default::default()
        };
        if let Err(e) = record(&r.db, &call) {
            eprintln!("chat recorder could not save a call: {e}");
        }
    }
}

/// One finished call, as seen by the relay.
#[derive(Default)]
pub struct Call<'a> {
    pub api: &'static str,
    pub path: &'a str,
    pub client: Option<String>,
    pub request: &'a [u8],
    /// The reply bytes kept for recording (at most `KEEP_LIMIT`).
    pub response: &'a [u8],
    /// The last bytes received (at most `TAIL_KEEP`); only used when `response` was cut short.
    pub tail: &'a [u8],
    pub content_type: Option<String>,
    pub status: Option<u16>,
    /// A real error: the model server could not be reached (status None) or answered with one.
    pub error: Option<String>,
    pub started: i64,
    /// When the connection to the model server closed (0 = when recorded).
    pub finished: i64,
    pub first_chunk_at: Option<i64>,
    pub last_chunk_at: Option<i64>,
    pub chunk_count: Option<i64>,
    pub bytes_received: Option<i64>,
    /// The chat app closed its connection before the reply ended (seen by the relay).
    pub downstream_closed: bool,
    /// Reading the reply from the model server failed partway.
    pub stream_error: Option<String>,
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
    /// The model server said the reply was finished: `data: [DONE]`, a finish_reason, Ollama
    /// `done: true`, or a final Responses API event. HTTP 200 alone is not enough.
    pub end_marker: bool,
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
                if d == "[DONE]" {
                    p.end_marker = true;
                    continue;
                }
                if d.is_empty() {
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
    p.end_marker |= p.finish_reason.is_some();
    p
}

/// True when the end of a reply (bytes from the middle of a stream onwards) holds the
/// end-of-reply marker. The first, probably partial, line is skipped.
fn tail_has_end_marker(api: &str, content_type: Option<&str>, tail: &[u8]) -> bool {
    let start = tail.iter().position(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    parse_response(api, content_type, &tail[start..]).end_marker
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

/// Longest name someone can give a chat recording (same as task names).
pub const NAME_CAP: usize = 120;

/// The name someone gave this chat recording, if any.
pub fn conversation_name(db: &Db, conversation_id: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(db.conn().query_row("SELECT name FROM relay_conversation_names WHERE conversation_id=?1", [conversation_id], |r| r.get(0)).optional()?)
}

/// Gives a chat recording its own name, or clears it (empty or only spaces) to go back to the
/// automatic name. Spaces are squeezed, the name is cut at `NAME_CAP` characters and anything that
/// looks like a secret is blanked, like automatic titles. Returns the saved name (None = cleared).
pub fn set_conversation_name(db: &Db, conversation_id: &str, name: &str) -> Result<Option<String>> {
    let conn = db.conn();
    let known: i64 = conn.query_row("SELECT COUNT(*) FROM relay_calls WHERE conversation_id=?1", [conversation_id], |r| r.get(0))?;
    if known == 0 {
        return Err(crate::RtError::NotFound(conversation_id.to_string()));
    }
    let squeezed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if squeezed.is_empty() {
        conn.execute("DELETE FROM relay_conversation_names WHERE conversation_id=?1", [conversation_id])?;
        return Ok(None);
    }
    let capped: String = squeezed.chars().take(NAME_CAP).collect();
    let saved = tracker_common::redaction::redact_text(&capped);
    conn.execute(
        "INSERT INTO relay_conversation_names (conversation_id, name, renamed_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(conversation_id) DO UPDATE SET name=excluded.name, renamed_at=excluded.renamed_at",
        rusqlite::params![conversation_id, saved, now_ms()],
    )?;
    Ok(Some(saved))
}

/// One user message as a one-line heading: spaces squeezed, cut at 80 characters with "...".
/// None when the message is empty.
pub fn short_question(text: &str) -> Option<String> {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    Some(if one_line.chars().count() > 80 { format!("{}...", one_line.chars().take(80).collect::<String>()) } else { one_line })
}

/// Builds the `relay_calls` row for one call (redacted).
pub fn build_row(c: &Call) -> Value {
    use tracker_common::redaction::{redact_deep, redact_text};
    let req: Value = serde_json::from_slice(c.request).unwrap_or(Value::Null);
    let msgs = request_messages(c.api, &req);
    let first_user = msgs.iter().find(|(r, _)| r == "user").map(|(_, t)| t.clone()).unwrap_or_default();
    let last_user = msgs.iter().rev().find(|(r, _)| r == "user").map(|(_, t)| t.clone()).unwrap_or_default();
    let title = redact_text(&short_question(&first_user).unwrap_or_else(|| "(no user message)".to_string()));
    let p = parse_response(c.api, c.content_type.as_deref(), c.response);
    let tool_calls = if p.tool_calls.is_empty() {
        Value::Null
    } else {
        redact_deep(&Value::Array(p.tool_calls.iter().map(|(id, n, a)| json!({"id": id, "name": n, "arguments": cap(a, DEEP_STRING_CAP)})).collect()))
    };
    let capture_cut = c.bytes_received.is_some_and(|b| b as usize > c.response.len());
    let end_marker = p.end_marker || (capture_cut && tail_has_end_marker(c.api, c.content_type.as_deref(), c.tail));
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
        "finished_at": if c.finished > 0 { c.finished } else { now_ms() },
        "end_marker": c.status.map(|_| end_marker),
        "downstream_closed": c.status.map(|_| c.downstream_closed),
        "stream_error": c.stream_error.as_deref().map(redact_text),
        "first_chunk_at": c.first_chunk_at,
        "last_chunk_at": c.last_chunk_at,
        "chunk_count": c.chunk_count,
        "bytes_received": c.bytes_received,
        "capture_cut": c.status.map(|_| capture_cut),
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


// ---- reading recordings back -----------------------------------------------------------------

fn is_tool_request_obj(v: &Value) -> bool {
    v.get("name").and_then(Value::as_str).is_some_and(|n| !n.trim().is_empty()) && v.get("arguments").is_some()
}

/// Tool names from JSON objects in `text` with "name" and "arguments" keys, bare or inside a
/// ```json fence: the model typed a tool request as text instead of making a real tool call.
pub fn typed_tool_requests(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !text.contains("\"name\"") || !text.contains("\"arguments\"") {
        return out;
    }
    // Try parsing a JSON value at each '{' (also finds objects nested in arrays or wrappers),
    // skipping past each one found. Capped so a huge reply full of braces stays cheap.
    let mut next = 0;
    for (i, _) in text.match_indices('{').take(200) {
        if i < next {
            continue;
        }
        let mut it = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            if is_tool_request_obj(&v) {
                out.push(v["name"].as_str().unwrap_or_default().to_string());
                next = i + it.byte_offset();
            }
        }
    }
    out
}

/// True when `text` holds a tool request typed as text (see `typed_tool_requests`).
pub fn looks_like_typed_tool_call(text: &str) -> bool {
    !typed_tool_requests(text).is_empty()
}

/// The message items of a request, in order (chat messages or Responses API input items).
fn request_items(api: &str, req: &Value) -> Vec<Value> {
    let key = if api == "responses" { "input" } else { "messages" };
    req[key].as_array().cloned().unwrap_or_default()
}

fn is_real_tool_item(v: &Value) -> bool {
    v["role"].as_str() == Some("tool") || matches!(v["type"].as_str(), Some("function_call") | Some("function_call_output"))
}

/// Earlier assistant turns in the request that typed a tool request as text, with the tool names
/// typed in each. A turn counts only when it has no `tool_calls` entry and isn't followed by a
/// tool result (a real tool call).
fn typed_turns_in_request(api: &str, req: &Value) -> Vec<Vec<String>> {
    let items = request_items(api, req);
    items
        .iter()
        .enumerate()
        .filter(|(i, m)| m["role"].as_str() == Some("assistant") && !m["tool_calls"].as_array().is_some_and(|a| !a.is_empty()) && !items.get(i + 1).is_some_and(is_real_tool_item))
        .map(|(_, m)| typed_tool_requests(&text_of(&m["content"])))
        .filter(|names| !names.is_empty())
        .collect()
}

/// How many earlier assistant turns in the request typed a tool request as text.
pub fn typed_tool_calls_in_request(api: &str, req: &Value) -> usize {
    typed_turns_in_request(api, req).len()
}

/// (reply typed a tool request as text with no real call, earlier assistant turns that did) for
/// one saved `relay_calls` row.
pub fn typed_tool_call_flags(call: &Value) -> (bool, usize) {
    let api = call["api"].as_str().unwrap_or("");
    let reply = call["reply_redacted"].as_str().unwrap_or("");
    let real_reply_calls = call["tool_calls"].as_array().is_some_and(|a| !a.is_empty());
    let in_reply = !real_reply_calls && looks_like_typed_tool_call(reply);
    (in_reply, typed_tool_calls_in_request(api, &call["request_redacted"]))
}

/// Most calls scanned for the list view's typed-tool-request counts. Only calls whose saved text
/// mentions "arguments" at all are scanned, newest first; past this cap counts may be too low.
const LIST_SCAN_CAP: i64 = 2000;

/// The chat recordings list: one row per conversation (newest first, at most 300), each with
/// `typed_tool_calls`, the number of its calls where the model typed a tool request as text
/// (same rule as `typed_tool_call` in the conversation view).
pub fn list_conversations(db: &Db) -> Result<Vec<Value>> {
    let conn = db.conn();
    let mut rows = store::rows(&conn, "SELECT c.conversation_id, COUNT(*) AS calls, MIN(c.started_at) AS first_at, MAX(c.started_at) AS last_at,
        (SELECT x.title FROM relay_calls x WHERE x.conversation_id=c.conversation_id ORDER BY x.started_at LIMIT 1) AS title,
        (SELECT x.model FROM relay_calls x WHERE x.conversation_id=c.conversation_id ORDER BY x.started_at DESC LIMIT 1) AS model,
        (SELECT x.client FROM relay_calls x WHERE x.conversation_id=c.conversation_id ORDER BY x.started_at DESC LIMIT 1) AS client,
        (SELECT x.last_user_redacted FROM relay_calls x WHERE x.conversation_id=c.conversation_id ORDER BY x.started_at DESC LIMIT 1) AS latest_user_full,
        (SELECT n.name FROM relay_conversation_names n WHERE n.conversation_id=c.conversation_id) AS custom_name,
        SUM(CASE WHEN c.reasoning_redacted IS NOT NULL THEN 1 ELSE 0 END) AS with_thinking,
        SUM(CASE WHEN c.tool_calls IS NOT NULL THEN 1 ELSE 0 END) AS with_tools
        FROM relay_calls c GROUP BY c.conversation_id ORDER BY last_at DESC LIMIT 300", &[])?;
    // Cheap pre-filter in SQL: a typed tool request always contains the word "arguments".
    let candidates = store::rows(&conn, "SELECT conversation_id, api, request_redacted, reply_redacted, tool_calls FROM relay_calls
        WHERE instr(COALESCE(reply_redacted, ''), 'arguments') > 0 OR instr(COALESCE(request_redacted, ''), 'arguments') > 0
        ORDER BY started_at DESC LIMIT ?1", &[&LIST_SCAN_CAP])?;
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for c in &candidates {
        let (in_reply, in_history) = typed_tool_call_flags(c);
        if in_reply || in_history > 0 {
            *counts.entry(c["conversation_id"].as_str().unwrap_or_default().to_string()).or_default() += 1;
        }
    }
    // Problems (real errors) and interrupted replies, by the same rule as the conversation view.
    let mut outcomes: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();
    for c in store::rows(&conn, "SELECT conversation_id, http_status, error, end_marker, finish_reason, downstream_closed FROM relay_calls", &[])? {
        let e = outcomes.entry(c["conversation_id"].as_str().unwrap_or_default().to_string()).or_default();
        match call_outcome(&c)["state"].as_str() {
            Some("problem") => e.0 += 1,
            Some("interrupted") => e.1 += 1,
            _ => {}
        }
    }
    for r in &mut rows {
        let id = r["conversation_id"].as_str().unwrap_or_default().to_string();
        r["typed_tool_calls"] = json!(counts.get(&id).copied().unwrap_or(0));
        let (problems, interrupted) = outcomes.get(&id).copied().unwrap_or_default();
        r["problems"] = json!(problems);
        r["errors"] = json!(problems);
        r["interrupted"] = json!(interrupted);
        // The newest question, already redacted when saved; cut like the title for the list.
        let latest = r["latest_user_full"].as_str().and_then(short_question);
        r["latest_question"] = json!(latest);
        r["latest_differs"] = json!(latest.as_deref().is_some_and(|q| Some(q) != r["title"].as_str()));
        if let Some(m) = r.as_object_mut() {
            m.remove("latest_user_full");
        }
    }
    Ok(rows)
}

/// Adds read-time fields to one `relay_calls` row for the conversation view:
/// - `latest_user`: the newest user message in the request;
/// - `latest_question`: the same as a one-line heading, and `latest_differs_from_title`;
/// - `earlier_messages`: the messages before it that the chat app re-sent ({role, text});
/// - `after_user_messages`: anything after it in the same request (for example tool results);
/// - `typed_tool_call_in_reply`: the reply typed a tool request as text and made no real call;
/// - `typed_tool_calls_in_history`: earlier assistant turns that did the same;
/// - `typed_tool_call`: either of the two.
/// Works on recordings made before this existed, since it only reads saved columns.
pub fn annotate_call(call: &mut Value) {
    let api = call["api"].as_str().unwrap_or("").to_string();
    let req = call["request_redacted"].clone();
    let msgs = request_messages(&api, &req);
    let last_user = msgs.iter().rposition(|(r, _)| r == "user");
    let latest_user = last_user.map(|i| msgs[i].1.clone()).or_else(|| call["last_user_redacted"].as_str().map(String::from));
    let item = |(r, t): &(String, String)| json!({"role": r, "text": t});
    let (earlier, after): (Vec<Value>, Vec<Value>) = match last_user {
        Some(i) => (msgs[..i].iter().map(item).collect(), msgs[i + 1..].iter().map(item).collect()),
        None => (msgs.iter().map(item).collect(), Vec::new()),
    };
    let (in_reply, in_history) = typed_tool_call_flags(call);
    // Heading for this message: its own newest question (cut like the title). Computed on read, so
    // older recordings get it too. `title` stays the chat's first question.
    let latest_question = latest_user.as_deref().and_then(short_question);
    call["latest_differs_from_title"] = json!(latest_question.as_deref().is_some_and(|q| Some(q) != call["title"].as_str()));
    call["latest_question"] = json!(latest_question);
    call["latest_user"] = json!(latest_user);
    call["earlier_messages"] = Value::Array(earlier);
    call["after_user_messages"] = Value::Array(after);
    call["typed_tool_call_in_reply"] = json!(in_reply);
    call["typed_tool_calls_in_history"] = json!(in_history);
    call["typed_tool_call"] = json!(in_reply || in_history > 0);
    call["outcome"] = call_outcome(call);
    call["request_size"] = request_size(call);
    call["shortened"] = Value::Array(shortened_parts(call));
    let tools = tool_split(call);
    for (k, v) in tools.as_object().into_iter().flatten() {
        call[k.as_str()] = v.clone();
    }
}

/// `annotate_call` for every call of one conversation (oldest first), then marks tool calls a reply
/// asked for as run when a later request carries a tool result with the same id.
pub fn annotate_conversation(calls: &mut [Value]) {
    calls.iter_mut().for_each(annotate_call);
    let run_ids: std::collections::HashSet<String> =
        calls.iter().flat_map(|c| c["tools_executed"].as_array().cloned().unwrap_or_default()).filter_map(|t| t["id"].as_str().map(String::from)).collect();
    for c in calls.iter_mut() {
        let have: Vec<String> = c["tools_executed"].as_array().into_iter().flatten().filter_map(|t| t["id"].as_str().map(String::from)).collect();
        let extra: Vec<Value> = c["tools_requested"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["where"] == "this reply")
            .filter(|t| t["id"].as_str().is_some_and(|id| run_ids.contains(id) && !have.iter().any(|h| h == id)))
            .map(|t| json!({"id": t["id"], "name": t["name"], "where": "result sent with a later message"}))
            .collect();
        if let Some(a) = c["tools_executed"].as_array_mut() {
            a.extend(extra);
        }
    }
}

/// The exact wording for a reply that ended without the end-of-reply marker and with no sign of why.
pub const CAUSE_UNKNOWN: &str = "Stream ended before completion; cause unknown.";
/// Notes saved in `error` by recordings made before the v008 columns existed. They describe an
/// unfinished reply, not a proxy or model server error.
const OLD_CHAT_APP_STOPPED: &str = "The chat app stopped the reply early";
const OLD_CUT_OFF: &str = "The model server's reply was cut off";

fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|x| x != 0),
        _ => None,
    }
}

/// How one call ended: `{state, label, cause, detail}`.
/// - `problem`: the model server couldn't be reached, answered with an HTTP error (400 or more),
///   or sent an error message;
/// - `complete`: the model server marked the reply finished (see `Parsed::end_marker`);
/// - `interrupted`: anything else, including HTTP 200 without the end marker. The chat app is
///   named as the cause only when the recorder saw it close its connection.
pub fn call_outcome(call: &Value) -> Value {
    let error = call["error"].as_str().unwrap_or("");
    let old_app = error.starts_with(OLD_CHAT_APP_STOPPED);
    let old_cut = error.starts_with(OLD_CUT_OFF);
    let status = call["http_status"].as_i64();
    match status {
        None => return json!({"state": "problem", "label": "No answer", "source": "no_answer", "kind": Value::Null,
            "cause": if error.is_empty() { "No answer from the model server." } else { error }, "detail": Value::Null}),
        Some(s) if s >= 400 => return server_error(call, if error.is_empty() { "The model server answered with an error." } else { error }),
        _ if !error.is_empty() && !old_app && !old_cut => return server_error(call, error),
        _ => {}
    }
    // Older recordings have no end_marker column value: a finish_reason is the marker there.
    let end = flag(&call["end_marker"]).unwrap_or(!call["finish_reason"].is_null());
    if end {
        return json!({"state": "complete", "label": "Finished", "cause": Value::Null, "detail": Value::Null});
    }
    // An older recording's "chat app stopped" note was only written when sending to the chat app failed.
    let downstream = flag(&call["downstream_closed"]).unwrap_or(old_app);
    let detail = call["stream_error"].as_str().map(String::from).or_else(|| old_cut.then(|| error.to_string()));
    let cause = if downstream {
        "Your chat app closed its connection before the reply finished (for example the Stop button, a closed tab, or the app giving up)."
    } else {
        CAUSE_UNKNOWN
    };
    json!({"state": "interrupted", "label": "Interrupted", "cause": cause, "by_chat_app": downstream, "detail": detail})
}

/// Error text from model servers that means the request didn't fit the loaded context window
/// (LM Studio, llama.cpp, Ollama and OpenAI-style wordings).
pub fn is_context_full_error(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    ["context size has been exceeded", "context length", "context window", "maximum context", "exceeds the context", "n_ctx", "too many tokens", "context_length_exceeded", "prompt is too long"]
        .iter()
        .any(|p| e.contains(p))
}

/// 12345 -> "12,345".
fn thousands(n: u64) -> String {
    let d = n.to_string();
    let mut out = String::new();
    for (i, ch) in d.chars().enumerate() {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// An error the model server itself sent (HTTP 400 or more, or an error message in the reply).
/// The server's own words go in `detail`; `cause` is a plain explanation where one is known.
fn server_error(call: &Value, error: &str) -> Value {
    // Checked first: a load failure can mention the context length in its advice.
    if is_model_load_error(error) {
        let mut cause = "The model server couldn't load this model: it is too big for this computer with the current settings.".to_string();
        if let Some(mem) = needed_memory(error) {
            cause.push_str(&format!(" It said the model needs about {mem} of memory."));
        }
        cause.push_str(" To fix it: pick a smaller model, or a smaller version of it (a lower quantization, such as Q4 instead of Q8), or lower the Context Length for this model in LM Studio. LM Studio can also turn off its loading limits, but that could make your PC freeze.");
        return json!({"state": "problem", "label": "Model server error", "source": "model_server", "kind": "model_load_failed", "cause": cause, "detail": error});
    }
    if is_context_full_error(error) {
        let size = request_size(call);
        let (lo, hi) = (size["est_tokens_low"].as_u64().unwrap_or(0), size["est_tokens_high"].as_u64().unwrap_or(0));
        let n = size["tools_count"].as_u64().unwrap_or(0);
        let mut cause = format!(
            "The request didn't fit in the model's context window (how much text the loaded model can hold at once). This request was roughly {}-{} tokens (rough estimate from its size).", thousands(lo), thousands(hi)
        );
        if n > 0 {
            let (tl, th) = (size["tools_tokens_low"].as_u64().unwrap_or(0), size["tools_tokens_high"].as_u64().unwrap_or(0));
            cause.push_str(&format!(" The {n} tool descriptions your chat app sent were about {}-{} of those.", thousands(tl), thousands(th)));
        }
        cause.push_str(" To fix it: raise Context Length in LM Studio's settings for this model and load it again, or turn off tools you don't need in your chat app. Thinking models also need room left over for their thinking and reply.");
        return json!({"state": "problem", "label": "Model server error", "source": "model_server", "kind": "context_full", "cause": cause, "detail": error});
    }
    json!({"state": "problem", "label": "Model server error", "source": "model_server", "kind": Value::Null, "cause": "The model server sent back an error instead of a reply.", "detail": error})
}

/// Error text meaning the model server couldn't load the model (LM Studio's loading guardrails,
/// out of memory while loading).
pub fn is_model_load_error(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    ["failed to load model", "model loading was stopped", "insufficient system resources", "error loading model", "failed to load the model", "not enough memory to load"]
        .iter()
        .any(|p| e.contains(p))
}

/// "approximately 117.94 GB" in an error -> "117.94 GB".
fn needed_memory(e: &str) -> Option<String> {
    let lower = e.to_ascii_lowercase();
    for key in ["approximately ", "about ", "requires "] {
        for (i, _) in lower.match_indices(key) {
            let rest = &e[i + key.len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
            if num.is_empty() || !num.chars().next().unwrap().is_ascii_digit() {
                continue;
            }
            let after = rest[num.len()..].trim_start();
            let unit: String = after.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            if ["GB", "GiB", "MB", "MiB", "TB"].iter().any(|u| u.eq_ignore_ascii_case(&unit)) {
                return Some(format!("{} {}", num.trim_end_matches(['.', ',']), unit));
            }
        }
    }
    None
}

/// Size of the saved request, split into tool descriptions and messages, with a rough token
/// range (3 to 4 characters per token). It's an estimate: the real count depends on the model.
pub fn request_size(call: &Value) -> Value {
    let api = call["api"].as_str().unwrap_or("");
    let req = &call["request_redacted"];
    let compact = |v: &Value| if v.is_null() { 0 } else { v.to_string().chars().count() as u64 };
    let tools = &req["tools"];
    let tools_chars = compact(tools);
    let messages_chars = compact(&Value::Array(request_items(api, req)));
    let total = compact(req);
    let est = |c: u64| (c / 4, c.div_ceil(3));
    let (lo, hi) = est(total);
    let (tl, th) = est(tools_chars);
    json!({"chars": total, "tools_count": tools.as_array().map(|a| a.len()).unwrap_or(0), "tools_chars": tools_chars, "messages_chars": messages_chars,
        "est_tokens_low": lo, "est_tokens_high": hi, "tools_tokens_low": tl, "tools_tokens_high": th})
}

/// True when `s` carries the note `cap()` adds: "[... N more characters not kept]".
fn has_cap_note(s: &str) -> bool {
    s.match_indices("[... ").any(|(i, _)| {
        let rest = &s[i + 5..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with(CAP_NOTE)
    })
}

/// Which saved parts were shortened, each `{part, limit, unit}`.
pub fn shortened_parts(call: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for (col, part) in [("reasoning_redacted", "thinking"), ("reply_redacted", "reply"), ("last_user_redacted", "your message")] {
        if call[col].as_str().is_some_and(has_cap_note) {
            out.push(json!({"part": part, "limit": TEXT_CAP, "unit": "characters"}));
        }
    }
    let req = &call["request_redacted"];
    if !req.is_null() && has_cap_note(&if let Some(s) = req.as_str() { s.to_string() } else { req.to_string() }) {
        out.push(json!({"part": "full request", "limit": DEEP_STRING_CAP, "unit": "characters"}));
    }
    if call["tool_calls"].as_array().into_iter().flatten().any(|t| t["arguments"].as_str().is_some_and(has_cap_note)) {
        out.push(json!({"part": "tool call details", "limit": DEEP_STRING_CAP, "unit": "characters"}));
    }
    if flag(&call["capture_cut"]) == Some(true) {
        out.push(json!({"part": "reply as received", "limit": KEEP_LIMIT >> 20, "unit": "MB"}));
    }
    out
}

/// Available, requested (real structured calls), typed-as-text and executed tools for one call.
/// "Executed" needs evidence: a tool result in the request whose id matches a tool call's id.
pub fn tool_split(call: &Value) -> Value {
    let api = call["api"].as_str().unwrap_or("");
    let req = &call["request_redacted"];
    let available: Vec<String> = req["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().or_else(|| t["name"].as_str()).map(String::from))
        .collect();
    let items = request_items(api, req);
    let mut requested = Vec::new();
    for it in &items {
        if it["role"].as_str() == Some("assistant") {
            for tc in it["tool_calls"].as_array().into_iter().flatten() {
                requested.push(json!({"id": tc["id"], "name": tc["function"]["name"].as_str().or_else(|| tc["name"].as_str()), "where": "earlier reply"}));
            }
        }
        if it["type"].as_str() == Some("function_call") {
            requested.push(json!({"id": it["call_id"], "name": it["name"], "where": "earlier reply"}));
        }
    }
    let reply_calls = call["tool_calls"].as_array().cloned().unwrap_or_default();
    for tc in &reply_calls {
        requested.push(json!({"id": tc["id"], "name": tc["name"], "where": "this reply"}));
    }
    let mut typed = Vec::new();
    for names in typed_turns_in_request(api, req) {
        typed.extend(names.into_iter().map(|n| json!({"name": n, "where": "earlier reply"})));
    }
    if reply_calls.is_empty() {
        typed.extend(typed_tool_requests(call["reply_redacted"].as_str().unwrap_or("")).into_iter().map(|n| json!({"name": n, "where": "this reply"})));
    }
    let mut executed = Vec::new();
    let mut unmatched = 0;
    for it in &items {
        let result_id = if it["role"].as_str() == Some("tool") {
            Some(it["tool_call_id"].as_str())
        } else if it["type"].as_str() == Some("function_call_output") {
            Some(it["call_id"].as_str())
        } else {
            None
        };
        let Some(id) = result_id else { continue };
        match id.and_then(|id| requested.iter().find(|r| r["where"] == "earlier reply" && r["id"].as_str() == Some(id))) {
            Some(r) => executed.push(json!({"id": r["id"], "name": r["name"], "where": "earlier reply"})),
            None => unmatched += 1,
        }
    }
    json!({"tools_available": available, "tools_requested": requested, "tools_typed_as_text": typed, "tools_executed": executed, "tool_results_unmatched": unmatched})
}
