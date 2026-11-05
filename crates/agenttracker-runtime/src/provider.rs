//! Provider adapters.
//!
//! `OpenAiCompatProvider` talks to a local OpenAI-compatible `/chat/completions` endpoint
//! (Ollama: `http://127.0.0.1:11434/v1`, LM Studio: `http://127.0.0.1:1234/v1`). Structured-output
//! and reasoning behaviour is NOT assumed identical across servers: the adapter records which
//! structured-output mode was accepted and whether any reasoning text was exposed.
//!
//! `ScriptedProvider` returns pre-recorded responses. It is used for simulated tests and for the
//! recorded UI fixtures; records produced with it are labelled as fixtures, never as live output.
use crate::config::{ProviderConfig, ProviderKind, StructuredMode};
use crate::{Result, RtError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    /// JSON schema the response should follow.
    pub schema: Value,
    /// Per-request seed override (investigation trials).
    pub seed: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    pub content: String,
    /// "provider-exposed reasoning", if the server returned any (optional).
    pub provider_reasoning: Option<String>,
    pub structured_mode_used: String,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub model_digest: Option<String>,
    pub model_details: Value,
    /// true for scripted fixtures / simulated providers.
    pub is_fixture: bool,
}

pub trait Provider: Send + Sync {
    fn info(&self) -> ProviderInfo;
    fn settings(&self) -> Value;
    fn complete(&self, req: &ChatRequest) -> Result<ChatResponse>;
}

// ---- OpenAI-compatible -------------------------------------------------------------------------------
pub struct OpenAiCompatProvider {
    pub cfg: ProviderConfig,
    agent: ureq::Agent,
    info: ProviderInfo,
    /// Mode that worked last time (Auto mode remembers it per provider instance).
    learned_mode: Mutex<Option<StructuredMode>>,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: ProviderConfig) -> Result<Self> {
        cfg.validate()?;
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(cfg.timeout_secs.max(5))).build();
        let mut p = Self {
            info: ProviderInfo { provider: cfg.kind.as_str().into(), endpoint: cfg.endpoint.clone(), model: cfg.model.clone(), model_digest: None, model_details: Value::Null, is_fixture: false },
            cfg,
            agent,
            learned_mode: Mutex::new(None),
        };
        let (digest, details) = p.lookup_model_metadata();
        p.info.model_digest = digest;
        p.info.model_details = details;
        Ok(p)
    }

    fn base(&self) -> String {
        self.cfg.endpoint.trim_end_matches('/').to_string()
    }

    fn native_root(&self) -> String {
        let b = self.base();
        b.strip_suffix("/v1").unwrap_or(&b).to_string()
    }

    fn get(&self, url: &str) -> Result<Value> {
        let mut r = self.agent.get(url);
        if let Some(k) = self.cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
            r = r.set("Authorization", &format!("Bearer {k}"));
        }
        r.call().map_err(|e| RtError::Provider(e.to_string()))?.into_json::<Value>().map_err(|e| RtError::Provider(e.to_string()))
    }

    /// Model digest/details where the server exposes them:
    /// - Ollama: `GET /api/tags` lists `digest` and `details` per model.
    /// - LM Studio: `GET /api/v0/models` lists arch/quantization/max_context_length (no digest).
    /// Anything unavailable is recorded as null rather than guessed.
    fn lookup_model_metadata(&self) -> (Option<String>, Value) {
        match self.cfg.kind {
            ProviderKind::Ollama => {
                if let Ok(v) = self.get(&format!("{}/api/tags", self.native_root())) {
                    if let Some(m) = v["models"].as_array().and_then(|a| a.iter().find(|m| m["name"] == self.cfg.model.as_str() || m["model"] == self.cfg.model.as_str())) {
                        return (m["digest"].as_str().map(String::from), m["details"].clone());
                    }
                }
                (None, Value::Null)
            }
            ProviderKind::LmStudio => {
                if let Ok(v) = self.get(&format!("{}/api/v0/models", self.native_root())) {
                    if let Some(m) = v["data"].as_array().and_then(|a| a.iter().find(|m| m["id"] == self.cfg.model.as_str())) {
                        return (None, json!({"arch": m["arch"], "quantization": m["quantization"], "max_context_length": m["max_context_length"], "state": m["state"]}));
                    }
                }
                (None, Value::Null)
            }
            ProviderKind::OpenAiCompatible => (None, Value::Null),
        }
    }

    /// Lists model ids from `GET {endpoint}/models`.
    pub fn list_models(&self) -> Result<Vec<String>> {
        let v = self.get(&format!("{}/models", self.base()))?;
        Ok(v["data"].as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect()).unwrap_or_default())
    }

    fn body(&self, req: &ChatRequest, mode: StructuredMode) -> Value {
        let mut b = json!({"model": self.cfg.model, "messages": req.messages, "stream": false, "max_tokens": self.cfg.max_tokens});
        if let Some(t) = self.cfg.temperature {
            b["temperature"] = json!(t);
        }
        if let Some(p) = self.cfg.top_p {
            b["top_p"] = json!(p);
        }
        if let Some(s) = req.seed.or(self.cfg.seed) {
            b["seed"] = json!(s);
        }
        match mode {
            StructuredMode::JsonSchema => b["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "decision_proposal", "strict": true, "schema": req.schema}}),
            StructuredMode::JsonObject => b["response_format"] = json!({"type": "json_object"}),
            _ => {}
        }
        b
    }

    fn post(&self, body: &Value) -> std::result::Result<Value, (Option<u16>, String)> {
        let mut r = self.agent.post(&format!("{}/chat/completions", self.base())).set("Content-Type", "application/json");
        if let Some(k) = self.cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
            r = r.set("Authorization", &format!("Bearer {k}"));
        }
        match r.send_json(body.clone()) {
            Ok(resp) => resp.into_json::<Value>().map_err(|e| (None, e.to_string())),
            Err(ureq::Error::Status(code, resp)) => Err((Some(code), resp.into_string().unwrap_or_default().chars().take(400).collect())),
            Err(e) => Err((None, e.to_string())),
        }
    }
}

impl Provider for OpenAiCompatProvider {
    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    fn settings(&self) -> Value {
        self.cfg.recorded_settings()
    }

    fn complete(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let modes: Vec<StructuredMode> = match self.cfg.structured_mode {
            StructuredMode::Auto => match *self.learned_mode.lock().unwrap_or_else(|p| p.into_inner()) {
                Some(m) => vec![m],
                None => vec![StructuredMode::JsonSchema, StructuredMode::JsonObject, StructuredMode::PromptOnly],
            },
            m => vec![m],
        };
        let mut last_err = String::new();
        for mode in modes {
            match self.post(&self.body(req, mode)) {
                Ok(v) => {
                    // Some servers answer HTTP 200 with an error body or an empty message (e.g. a prompt
                    // longer than the loaded context). Treat that as a problem, not as a model reply.
                    if let Some(problem) = empty_reply_problem(&v) {
                        last_err = format!("{} mode: {problem}", mode_name(mode));
                        if self.cfg.structured_mode == StructuredMode::Auto {
                            continue;
                        }
                        return Err(RtError::Provider(last_err));
                    }
                    let msg = &v["choices"][0]["message"];
                    let content = msg["content"].as_str().unwrap_or("").to_string();
                    // Servers differ: some expose `reasoning_content`, some `reasoning`, most nothing.
                    let reasoning = msg["reasoning_content"].as_str().or_else(|| msg["reasoning"].as_str()).filter(|s| !s.trim().is_empty()).map(String::from);
                    if self.cfg.structured_mode == StructuredMode::Auto {
                        *self.learned_mode.lock().unwrap_or_else(|p| p.into_inner()) = Some(mode);
                    }
                    return Ok(ChatResponse { content, provider_reasoning: reasoning, structured_mode_used: mode_name(mode).into(), raw: v });
                }
                // 400/422: the server rejected this response_format; try the next mode.
                Err((Some(code), m)) if code == 400 || code == 422 => last_err = format!("HTTP {code}: {m}"),
                Err((code, m)) => return Err(RtError::Provider(format!("{}{m}", code.map(|c| format!("HTTP {c}: ")).unwrap_or_default()))),
            }
        }
        Err(RtError::Provider(last_err))
    }
}

/// Returns a plain-language problem when a successful HTTP response carries no usable reply.
pub fn empty_reply_problem(v: &Value) -> Option<String> {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        let text = e.get("message").and_then(Value::as_str).map(String::from).or_else(|| e.as_str().map(String::from)).unwrap_or_else(|| e.to_string());
        return Some(format!("the server reported an error: {}", text.chars().take(400).collect::<String>()));
    }
    let Some(choice) = v["choices"].get(0) else {
        return Some("the server's answer had no reply in it".into());
    };
    let content = choice["message"]["content"].as_str().unwrap_or("");
    if content.trim().is_empty() {
        let finish = choice["finish_reason"].as_str().unwrap_or("unknown");
        let hint = if finish == "length" { " (it ran out of room; try a larger Max output tokens or Context Length)" } else { "" };
        return Some(format!("the model sent back an empty reply, finish reason: {finish}{hint}"));
    }
    None
}

pub fn mode_name(m: StructuredMode) -> &'static str {
    match m {
        StructuredMode::Auto => "auto",
        StructuredMode::JsonSchema => "json_schema",
        StructuredMode::JsonObject => "json_object",
        StructuredMode::PromptOnly => "prompt_only",
    }
}

// ---- Scripted (fixtures / simulation) -----------------------------------------------------------------
type Script = Box<dyn Fn(&ChatRequest, usize) -> Result<ChatResponse> + Send + Sync>;

/// Returns scripted responses. `label` is recorded as the provider name, e.g.
/// "fixture (recorded, not a live model)" or "simulated".
pub struct ScriptedProvider {
    label: String,
    model: String,
    calls: Mutex<usize>,
    script: Script,
}

impl ScriptedProvider {
    /// Responses are returned in order; after the last one the last response repeats.
    pub fn sequence(label: &str, responses: Vec<String>) -> Self {
        Self::from_fn(label, move |_, i| Ok(scripted(&responses[i.min(responses.len().saturating_sub(1))], None)))
    }

    pub fn from_fn(label: &str, f: impl Fn(&ChatRequest, usize) -> Result<ChatResponse> + Send + Sync + 'static) -> Self {
        Self { label: label.into(), model: "scripted".into(), calls: Mutex::new(0), script: Box::new(f) }
    }

    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub fn scripted(content: &str, reasoning: Option<&str>) -> ChatResponse {
    ChatResponse { content: content.into(), provider_reasoning: reasoning.map(String::from), structured_mode_used: "scripted".into(), raw: Value::Null }
}

impl Provider for ScriptedProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: self.label.clone(), endpoint: "none (scripted)".into(), model: self.model.clone(), model_digest: None, model_details: Value::Null, is_fixture: true }
    }

    fn settings(&self) -> Value {
        json!({"scripted": true})
    }

    fn complete(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let i = {
            let mut c = self.calls.lock().unwrap_or_else(|p| p.into_inner());
            *c += 1;
            *c - 1
        };
        (self.script)(req, i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    #[test]
    fn empty_reply_problem_spots_errors_and_empty_content() {
        assert!(empty_reply_problem(&json!({"error": "context overflow"})).unwrap().contains("context overflow"));
        assert!(empty_reply_problem(&json!({"error": {"message": "too long"}})).unwrap().contains("too long"));
        assert!(empty_reply_problem(&json!({"choices": []})).is_some());
        assert!(empty_reply_problem(&json!({"choices": [{"message": {"content": "  "}, "finish_reason": "length"}]})).unwrap().contains("ran out of room"));
        assert!(empty_reply_problem(&json!({"choices": [{"message": {"content": "{\"ok\":true}"}, "finish_reason": "stop"}]})).is_none());
    }

    /// Fake server: empty reply when `response_format` is json_schema, 400 for json_object (like LM Studio),
    /// and a real reply with no response_format. Returns (endpoint, received modes).
    fn fake_server(n: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..n {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if line.trim().is_empty() { break; }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { len = v.trim().parse().unwrap(); }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let v: Value = serde_json::from_slice(&body).unwrap();
                let mode = v["response_format"]["type"].as_str().unwrap_or("none").to_string();
                let (code, out) = match mode.as_str() {
                    "json_schema" => (200, json!({"choices": [{"message": {"content": ""}, "finish_reason": "stop"}]})),
                    "json_object" => (400, json!({"error": "'response_format.type' must be 'json_schema' or 'text'"})),
                    _ => (200, json!({"choices": [{"message": {"content": "{\"ok\":true}"}, "finish_reason": "stop"}]})),
                };
                seen.push(mode);
                let b = out.to_string();
                write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len()).unwrap();
            }
            seen
        });
        (format!("http://127.0.0.1:{port}/v1"), h)
    }

    fn cfg(endpoint: String, mode: StructuredMode) -> ProviderConfig {
        ProviderConfig { kind: ProviderKind::OpenAiCompatible, endpoint, model: "m".into(), structured_mode: mode, ..Default::default() }
    }

    fn req() -> ChatRequest {
        ChatRequest { messages: vec![ChatMessage { role: "user".into(), content: "hi".into() }], schema: json!({"type": "object"}), seed: None }
    }

    #[test]
    fn auto_moves_past_empty_schema_reply_and_remembers_prompt_only() {
        let (ep, h) = fake_server(4);
        let p = OpenAiCompatProvider::new(cfg(ep, StructuredMode::Auto)).unwrap();
        let r = p.complete(&req()).unwrap();
        assert_eq!(r.content, "{\"ok\":true}");
        assert_eq!(r.structured_mode_used, "prompt_only");
        // Second call goes straight to the mode that worked.
        assert_eq!(p.complete(&req()).unwrap().structured_mode_used, "prompt_only");
        assert_eq!(h.join().unwrap(), vec!["json_schema", "json_object", "none", "none"]);
    }

    #[test]
    fn fixed_schema_mode_reports_empty_reply_plainly() {
        let (ep, h) = fake_server(1);
        let p = OpenAiCompatProvider::new(cfg(ep, StructuredMode::JsonSchema)).unwrap();
        let e = p.complete(&req()).unwrap_err().to_string();
        assert!(e.contains("empty reply"), "{e}");
        h.join().unwrap();
    }
}
