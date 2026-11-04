//! Provider and runtime configuration. Local by default: non-loopback endpoints are refused unless
//! `allow_remote` is explicitly set.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Ollama,
    LmStudio,
    OpenAiCompatible,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Ollama => "ollama",
            ProviderKind::LmStudio => "lmstudio",
            ProviderKind::OpenAiCompatible => "openai_compatible",
        }
    }
}

/// How the runtime asks for structured output. Behaviour differs between servers and models, so
/// `Auto` tries `json_schema`, then `json_object`, then prompt-only and records which one worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredMode {
    Auto,
    JsonSchema,
    JsonObject,
    PromptOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// Base URL of the OpenAI-compatible API, e.g. `http://127.0.0.1:11434/v1` (Ollama) or
    /// `http://127.0.0.1:1234/v1` (LM Studio).
    pub endpoint: String,
    pub model: String,
    /// Bearer token if the server requires one. Never serialised into records or API responses.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub seed: Option<i64>,
    #[serde(default = "d_max_tokens")]
    pub max_tokens: u32,
    /// Approximate context budget in characters for runtime-side compaction (the runtime does not
    /// see the provider's tokenizer; this is an application-level estimate).
    #[serde(default = "d_context_chars")]
    pub context_limit_chars: usize,
    #[serde(default = "d_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "d_mode")]
    pub structured_mode: StructuredMode,
    #[serde(default)]
    pub allow_remote: bool,
}

fn d_max_tokens() -> u32 {
    700
}
fn d_context_chars() -> usize {
    24_000
}
fn d_timeout() -> u64 {
    300
}
fn d_mode() -> StructuredMode {
    StructuredMode::Auto
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Ollama,
            endpoint: "http://127.0.0.1:11434/v1".into(),
            model: String::new(),
            api_key: None,
            temperature: Some(0.2),
            top_p: None,
            seed: None,
            max_tokens: d_max_tokens(),
            context_limit_chars: d_context_chars(),
            timeout_secs: d_timeout(),
            structured_mode: StructuredMode::Auto,
            allow_remote: false,
        }
    }
}

impl ProviderConfig {
    pub fn validate(&self) -> crate::Result<()> {
        let ep = self.endpoint.trim();
        let rest = ep.strip_prefix("http://").or_else(|| ep.strip_prefix("https://")).ok_or_else(|| crate::RtError::Invalid("Endpoint must start with http:// or https://".into()))?;
        let host_port = rest.split('/').next().unwrap_or("");
        let host = if host_port.starts_with('[') { host_port.split(']').next().unwrap_or("").trim_start_matches('[') } else { host_port.split(':').next().unwrap_or("") };
        let local = matches!(host, "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.");
        if !local && !self.allow_remote {
            return Err(crate::RtError::Invalid(format!("Endpoint host '{host}' is not on this computer. Enable 'allow remote' explicitly to use it.")));
        }
        if self.model.trim().is_empty() {
            return Err(crate::RtError::Invalid("Choose a model".into()));
        }
        Ok(())
    }

    /// Settings recorded with each request (no secrets).
    pub fn recorded_settings(&self) -> serde_json::Value {
        serde_json::json!({
            "temperature": self.temperature, "top_p": self.top_p, "seed": self.seed,
            "max_tokens": self.max_tokens, "context_limit_chars": self.context_limit_chars,
            "timeout_secs": self.timeout_secs, "structured_mode": self.structured_mode,
            "auth": if self.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false) { "bearer (value not recorded)" } else { "none" },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default = "d_retries")]
    pub max_validation_retries: u32,
    #[serde(default = "d_steps")]
    pub max_steps: u32,
    #[serde(default = "d_tool_calls")]
    pub max_tool_calls: u32,
    /// Tools that need explicit user approval before dispatch.
    #[serde(default = "d_approval")]
    pub approval_required_tools: Vec<String>,
}

fn d_retries() -> u32 {
    2
}
fn d_steps() -> u32 {
    6
}
fn d_tool_calls() -> u32 {
    4
}
fn d_approval() -> Vec<String> {
    vec!["write_text_file".into()]
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_validation_retries: d_retries(), max_steps: d_steps(), max_tool_calls: d_tool_calls(), approval_required_tools: d_approval() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub limits: Limits,
    /// Explicit opt-in: keep exact (unredacted) request payloads in the encrypted replay store.
    #[serde(default)]
    pub replay_capture: bool,
    #[serde(default = "d_retention")]
    pub replay_retention_days: u32,
}

fn d_retention() -> u32 {
    7
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self { provider: ProviderConfig::default(), limits: Limits::default(), replay_capture: false, replay_retention_days: d_retention() }
    }
}
