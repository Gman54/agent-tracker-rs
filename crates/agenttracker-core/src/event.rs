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

/// The time span (start, end in ms) that belongs to one task, so earlier and later requests in
/// the same agent session don't get mixed in. Starts at the request that opened the task (or the
/// task's creation) and ends shortly after its check, or at the next request after the check began.
pub fn task_window(created_at: i64, run_started: i64, run_finished: Option<i64>, events: &[AgentEvent]) -> (i64, i64) {
    const SLACK_MS: i64 = 2_000;
    const AFTER_MS: i64 = 60_000;
    let ts = |e: &AgentEvent| e.source_ts.unwrap_or(e.collector_ts);
    let prompts: Vec<i64> = events.iter().filter(|e| e.event_type == EventType::TaskSubmitted).map(ts).collect();
    // The request that opened an automatic task arrives just before the task is created.
    let opener = prompts.iter().copied().filter(|&t| t <= created_at + SLACK_MS && t >= created_at - 10_000).max();
    let start = opener.unwrap_or(created_at).min(created_at) - SLACK_MS;
    let finished = run_finished.unwrap_or_else(crate::now_ms).max(run_started);
    let next = prompts.iter().copied().filter(|&t| t > run_started.max(created_at + SLACK_MS)).min();
    let end = match next {
        Some(n) => (finished + AFTER_MS).min(n - 1),
        None => finished + AFTER_MS,
    };
    (start, end.max(start))
}

/// The window for a task page: up to now while it runs, else up to when it was checked.
pub fn window_for_task_page(task: &crate::task::TaskDefinition, events: &[AgentEvent]) -> (i64, i64) {
    let until = if task.lifecycle == crate::task::Lifecycle::Running { crate::now_ms() } else { task.updated_at };
    task_window(task.created_at, until, Some(until), events)
}

/// Keeps only the events inside the task's window. If that would leave nothing, keeps them all.
pub fn only_this_task(events: Vec<AgentEvent>, window: (i64, i64)) -> Vec<AgentEvent> {
    let inside = |e: &AgentEvent| {
        let t = e.source_ts.unwrap_or(e.collector_ts);
        t >= window.0 && t <= window.1
    };
    if events.iter().any(inside) {
        events.into_iter().filter(inside).collect()
    } else {
        events
    }
}

/// Sorts events in source order within a session: source_seq (missing last), then source_ts,
/// then collector_ts, then event_id for stability.
pub fn sort_source_order(events: &mut [AgentEvent]) {
    events.sort_by(|a, b| {
        let k = |e: &AgentEvent| (e.source_seq.is_none(), e.source_seq.unwrap_or(0), e.source_ts.unwrap_or(i64::MAX), e.collector_ts);
        k(a).cmp(&k(b)).then_with(|| a.event_id.cmp(&b.event_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: EventType, ts: i64) -> AgentEvent {
        let mut e = AgentEvent::new("claude_code", "1", t, CaptureSource::Hook, None);
        e.source_ts = Some(ts);
        e.collector_ts = ts + 200;
        e
    }

    // Times from a real session: three earlier requests, then the one this task is about.
    fn session() -> Vec<AgentEvent> {
        use EventType::*;
        vec![
            ev(SessionStarted, 1794062648662),
            ev(TaskSubmitted, 1794062651594),
            ev(ToolStarted, 1794062653359),
            ev(AgentReportedCompletion, 1794062658269),
            ev(TaskSubmitted, 1794066673821),
            ev(ToolStarted, 1794066675743),
            ev(AgentReportedCompletion, 1794066679541),
            ev(TaskSubmitted, 1794067522998),
            ev(AgentReportedCompletion, 1794067530331),
            ev(TaskSubmitted, 1794067851990),
        ]
    }

    #[test]
    fn earlier_requests_in_the_session_are_left_out() {
        let events = session();
        let w = task_window(1794067852211, 1794067983474, Some(1794068004846), &events);
        let kept = only_this_task(events, w);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].source_ts, Some(1794067851990));
    }

    #[test]
    fn a_middle_task_stops_at_the_next_request() {
        let events = session();
        // The "erase" task: created right after its request, checked about 25 seconds later.
        let w = task_window(1794066674287, 1794066702043, Some(1794066723452), &events);
        let kept = only_this_task(events, w);
        let times: Vec<_> = kept.iter().filter_map(|e| e.source_ts).collect();
        assert_eq!(times, vec![1794066673821, 1794066675743, 1794066679541]);
    }

    #[test]
    fn a_later_request_soon_after_the_check_is_not_pulled_in() {
        let mut events = session();
        events.push(ev(EventType::TaskSubmitted, 1794068010000));
        let w = task_window(1794067852211, 1794067983474, Some(1794068004846), &events);
        assert!(w.1 < 1794068010000);
    }
}
