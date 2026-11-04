//! Normalized agent event model (spec §4). Every connector converts its native payloads into
//! [`AgentEvent`]; payloads are redacted before they are stored.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    SessionStarted,
    TaskSubmitted,
    ToolStarted,
    ToolFinished,
    AgentReportedCompletion,
    SessionEnded,
    CaptureError,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStarted => "session_started",
            Self::TaskSubmitted => "task_submitted",
            Self::ToolStarted => "tool_started",
            Self::ToolFinished => "tool_finished",
            Self::AgentReportedCompletion => "agent_reported_completion",
            Self::SessionEnded => "session_ended",
            Self::CaptureError => "capture_error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.to_string())).ok()
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    Hook,
    Polling,
    Manual,
}

impl CaptureSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Polling => "polling",
            Self::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.to_string())).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub event_id: String,
    pub schema_version: String,
    pub connector_name: String,
    pub connector_version: String,
    pub agent_version: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub parent_id: Option<String>,
    /// Timestamp reported by the agent/hook (ms). None when the source has no clock value.
    pub source_ts: Option<i64>,
    /// Time the collector received the event (ms). Always collector-generated.
    pub collector_ts: i64,
    /// Source ordering key within a session; ordering uses this before collector_ts.
    pub source_seq: Option<i64>,
    pub working_dir: Option<String>,
    pub event_type: EventType,
    pub payload_redacted: Value,
    pub capture_source: CaptureSource,
    pub raw_source: Option<Value>,
    /// True when the collector had to assign fields the source did not supply (event_id).
    /// `collector_ts` is always collector-generated and is not counted here.
    pub collector_generated: bool,
}

impl AgentEvent {
    /// Builds an event, assigning a collector event id when the source did not provide one.
    pub fn new(connector_name: &str, connector_version: &str, event_type: EventType, capture_source: CaptureSource, source_event_id: Option<String>) -> Self {
        let collector_generated = source_event_id.is_none();
        Self {
            event_id: source_event_id.unwrap_or_else(|| format!("col-{}", uuid::Uuid::new_v4())),
            schema_version: crate::EVENT_SCHEMA_VERSION.to_string(),
            connector_name: connector_name.to_string(),
            connector_version: connector_version.to_string(),
            agent_version: None,
            session_id: None,
            task_id: None,
            tool_call_id: None,
            parent_id: None,
            source_ts: None,
            collector_ts: crate::now_ms(),
            source_seq: None,
            working_dir: None,
            event_type,
            payload_redacted: Value::Null,
            capture_source,
            raw_source: None,
            collector_generated,
        }
    }

    /// Re-applies redaction to the payload and raw source (idempotent).
    pub fn redact(&mut self) {
        self.payload_redacted = tracker_common::redaction::redact_deep(&self.payload_redacted);
        if let Some(raw) = &self.raw_source {
            self.raw_source = Some(tracker_common::redaction::redact_deep(raw));
        }
    }
}

/// Deterministic event id for sources that don't provide one but whose payload is unique
/// (so a re-delivered hook payload deduplicates). Hash of connector + canonical payload.
pub fn content_event_id(connector: &str, payload: &Value) -> String {
    let h = tracker_common::canonical::canonical_hash(&serde_json::json!({"c": connector, "p": payload}));
    format!("{connector}-{}", &hex::encode(h)[..32])
}

/// Sorts events in source order within a session: source_seq (missing last), then source_ts,
/// then collector_ts, then event_id for stability.
pub fn sort_source_order(events: &mut [AgentEvent]) {
    events.sort_by(|a, b| {
        let k = |e: &AgentEvent| (e.source_seq.is_none(), e.source_seq.unwrap_or(0), e.source_ts.unwrap_or(i64::MAX), e.collector_ts);
        k(a).cmp(&k(b)).then_with(|| a.event_id.cmp(&b.event_id))
    });
}
