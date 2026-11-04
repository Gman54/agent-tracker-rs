//! Claude Code connector.
//!
//! Status: **Implemented, requires live Claude Code for full validation. Hook format based on the
//! documented API** (https://docs.anthropic.com/en/docs/claude-code/hooks, checked 2026-11-03).
//!
//! Config: `~/.claude/settings.json` (`%USERPROFILE%\.claude\settings.json` on Windows):
//! `{"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "..."}]}]}}`
//! The hook receives JSON on stdin with `session_id`, `transcript_path`, `cwd`, `hook_event_name`
//! and event-specific fields (`tool_name`, `tool_input`, `tool_use_id`, `tool_response`, `prompt`).
use super::json_hooks::{self, HooksFormat, Upsert};
use super::{
    base_event, hook_binary_step, hook_path_from_command, probe_version, read_json_file, Capability, ConnectionTest, ConnectorInfo, DetectResult,
    HookEnvelope, InstallReport, Maturity, TestStep, UninstallReport,
};
use crate::event::{AgentEvent, EventType};
use crate::Result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "claude_code";

/// Hook events AgentTracker installs. Tool events use matcher "*".
pub const HOOK_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop", "SessionEnd"];
const TOOL_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];

pub fn info() -> ConnectorInfo {
    ConnectorInfo {
        name: NAME,
        display_name: "Claude Code",
        maturity: Maturity::Implemented,
        status_note: "Implemented, requires live Claude Code for full validation. Hook format based on documented API.",
        docs_checked: "https://docs.anthropic.com/en/docs/claude-code/hooks (checked 2026-11-03)",
    }
}

struct ClaudeFormat;

impl HooksFormat for ClaudeFormat {
    fn upsert(&self, hooks: &mut Map<String, Value>, event: &str, cmd: &str) -> Upsert {
        let arr = hooks.entry(event).or_insert_with(|| Value::Array(vec![]));
        if !arr.is_array() {
            *arr = Value::Array(vec![]);
        }
        let arr = arr.as_array_mut().expect("array");
        for group in arr.iter_mut() {
            if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                for h in inner.iter_mut() {
                    if h.get("command").and_then(Value::as_str).map(super::is_owned_command).unwrap_or(false) {
                        if h["command"] == cmd {
                            return Upsert::Unchanged;
                        }
                        h["command"] = Value::String(cmd.to_string());
                        return Upsert::Updated;
                    }
                }
            }
        }
        let hook = json!({"type": "command", "command": cmd, "timeout": 5});
        let group = if TOOL_EVENTS.contains(&event) { json!({"matcher": "*", "hooks": [hook]}) } else { json!({"hooks": [hook]}) };
        arr.push(group);
        Upsert::Added
    }

    fn remove_owned(&self, arr: &mut Vec<Value>) -> usize {
        let mut removed = 0;
        for group in arr.iter_mut() {
            if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|h| !h.get("command").and_then(Value::as_str).map(super::is_owned_command).unwrap_or(false));
                removed += before - inner.len();
            }
        }
        arr.retain(|g| g.get("hooks").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(true));
        removed
    }

    fn commands(&self, arr: &[Value]) -> Vec<String> {
        arr.iter()
            .filter_map(|g| g.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter_map(|h| h.get("command").and_then(Value::as_str).map(str::to_string))
            .collect()
    }
}

pub struct ClaudeConnector {
    pub settings_path: PathBuf,
    pub state_dir: PathBuf,
}

impl ClaudeConnector {
    /// User-level settings (`~/.claude/settings.json`).
    pub fn for_user() -> Self {
        Self { settings_path: crate::user_home().join(".claude").join("settings.json"), state_dir: crate::agenttracker_home().join("connectors") }
    }

    pub fn with_paths(settings_path: PathBuf, state_dir: PathBuf) -> Self {
        Self { settings_path, state_dir }
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir.join("claude_code_install.json")
    }

    /// Alternative locations we look at (some Windows setups keep config under %APPDATA%).
    fn alternate_paths() -> Vec<PathBuf> {
        let mut v = vec![];
        if let Some(a) = std::env::var_os("APPDATA") {
            v.push(PathBuf::from(a).join("claude").join("settings.json"));
        }
        v
    }

    pub fn detect(&self) -> DetectResult {
        let version = probe_version(&["claude", "claude.cmd", "claude.exe"], Duration::from_secs(4));
        let config_exists = self.settings_path.exists();
        let mut notes = vec![];
        if !config_exists {
            for alt in Self::alternate_paths() {
                if alt.exists() {
                    notes.push(format!("Found a Claude settings file at {} (AgentTracker uses {})", alt.display(), self.settings_path.display()));
                }
            }
        }
        if version.is_none() {
            notes.push("The `claude` command wasn't found on PATH. Hooks can still be installed; they take effect when Claude Code runs.".into());
        }
        let hooks_installed = read_json_file(&self.settings_path).ok().flatten().map(|c| !json_hooks::owned_events(&ClaudeFormat, &c).is_empty()).unwrap_or(false);
        DetectResult {
            connector: NAME.into(),
            agent_found: version.is_some(),
            agent_version: version,
            config_path: self.settings_path.to_string_lossy().into_owned(),
            config_exists,
            hooks_installed,
            notes,
        }
    }

    pub fn install(&self, hook_binary_path: &Path) -> Result<InstallReport> {
        json_hooks::install(&ClaudeFormat, NAME, &self.settings_path, &self.state_file(), HOOK_EVENTS, hook_binary_path)
    }

    pub fn uninstall(&self) -> Result<UninstallReport> {
        json_hooks::uninstall(&ClaudeFormat, &self.settings_path, &self.state_file())
    }

    pub fn connection_test(&self) -> ConnectionTest {
        let mut steps = vec![];
        let cfg = match read_json_file(&self.settings_path) {
            Ok(Some(c)) => {
                steps.push(TestStep { name: "Settings file".into(), ok: true, detail: format!("{} is valid JSON", self.settings_path.display()) });
                Some(c)
            }
            Ok(None) => {
                steps.push(TestStep { name: "Settings file".into(), ok: false, detail: format!("{} doesn't exist", self.settings_path.display()) });
                None
            }
            Err(e) => {
                steps.push(TestStep { name: "Settings file".into(), ok: false, detail: e.to_string() });
                None
            }
        };
        let owned = cfg.as_ref().map(|c| json_hooks::owned_events(&ClaudeFormat, c)).unwrap_or_default();
        let missing: Vec<&str> = HOOK_EVENTS.iter().copied().filter(|e| !owned.iter().any(|(oe, _)| oe == e)).collect();
        steps.push(TestStep {
            name: "AgentTracker hooks".into(),
            ok: missing.is_empty(),
            detail: if missing.is_empty() { format!("{} hooks installed", owned.len()) } else { format!("Missing: {}", missing.join(", ")) },
        });
        steps.push(hook_binary_step(owned.first().and_then(|(_, c)| hook_path_from_command(c))));
        ConnectionTest { ok: steps.iter().all(|s| s.ok), steps }
    }

    pub fn capabilities() -> Vec<Capability> {
        let c = |t, available, d: &str| Capability { event_type: t, available, detail: d.into() };
        vec![
            c(EventType::SessionStarted, true, "SessionStart hook"),
            c(EventType::TaskSubmitted, true, "UserPromptSubmit hook (prompt text is redacted before storage)"),
            c(EventType::ToolStarted, true, "PreToolUse hook (all tools, matcher \"*\")"),
            c(EventType::ToolFinished, true, "PostToolUse hook"),
            c(EventType::AgentReportedCompletion, true, "Stop hook — the agent *says* it is done; files are verified separately"),
            c(EventType::SessionEnded, true, "SessionEnd hook"),
            c(EventType::CaptureError, true, "Generated by AgentTracker when a payload can't be understood"),
        ]
    }
}

/// Maps a Claude Code hook delivery to a normalized event. Notification hooks only refresh
/// connector health (there is no matching normalized type) and return None.
pub fn normalize(env: &HookEnvelope) -> Option<AgentEvent> {
    let p = &env.payload;
    let name = p.get("hook_event_name").and_then(Value::as_str).unwrap_or(&env.hook_event);
    let t = match name {
        "SessionStart" => EventType::SessionStarted,
        "UserPromptSubmit" => EventType::TaskSubmitted,
        "PreToolUse" => EventType::ToolStarted,
        "PostToolUse" | "PostToolUseFailure" => EventType::ToolFinished,
        "Stop" => EventType::AgentReportedCompletion,
        "SessionEnd" => EventType::SessionEnded,
        "Notification" => return None,
        _ => EventType::CaptureError,
    };
    let mut ev = base_event(env, t);
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    ev.session_id = s("session_id");
    ev.working_dir = s("cwd");
    ev.tool_call_id = s("tool_use_id");
    ev.payload_redacted = match t {
        EventType::ToolStarted => json!({"tool": p.get("tool_name"), "input": p.get("tool_input")}),
        EventType::ToolFinished => json!({"tool": p.get("tool_name"), "input": p.get("tool_input"), "response": p.get("tool_response"), "failed": name == "PostToolUseFailure"}),
        EventType::TaskSubmitted => json!({"prompt": p.get("prompt")}),
        EventType::SessionStarted => json!({"source": p.get("source")}),
        EventType::SessionEnded => json!({"reason": p.get("reason")}),
        EventType::AgentReportedCompletion => json!({"stop_hook_active": p.get("stop_hook_active")}),
        EventType::CaptureError => json!({"error": format!("unrecognized Claude Code hook \"{name}\""), "payload": p}),
    };
    ev.redact();
    Some(ev)
}
