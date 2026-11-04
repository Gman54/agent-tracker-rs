//! Cursor connector.
//!
//! Status: **Experimental — Cursor hook API not fully validated at time of implementation.
//! Tested: config file detection, install/uninstall merging and payload normalization with fake
//! payloads only. Not tested against a live Cursor.**
//! Docs checked: https://cursor.com/docs/agent/hooks (Cursor "Hooks", fetched 2026-11-03; the page
//! carries no explicit version number). Config: `~/.cursor/hooks.json`:
//! `{"version": 1, "hooks": {"afterFileEdit": [{"command": "..."}]}}`. Payload common fields:
//! `conversation_id`, `generation_id`, `hook_event_name`, `cursor_version`, `workspace_roots`.
//!
//! Only observational hooks are installed. "before*" / `preToolUse` hooks can return permission
//! decisions; AgentTracker never blocks, so it does not register those (the TaskSubmitted and
//! ToolStarted capabilities are therefore marked unavailable).
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

pub const NAME: &str = "cursor";

pub const HOOK_EVENTS: &[&str] = &["sessionStart", "postToolUse", "postToolUseFailure", "afterShellExecution", "afterMCPExecution", "afterFileEdit", "stop", "sessionEnd"];

pub fn info() -> ConnectorInfo {
    ConnectorInfo {
        name: NAME,
        display_name: "Cursor",
        maturity: Maturity::Experimental,
        status_note: "Experimental — Cursor hook API not fully validated at time of implementation. Tested: config file detection and config merging only.",
        docs_checked: "https://cursor.com/docs/agent/hooks (fetched 2026-11-03, no version number published)",
    }
}

struct CursorFormat;

impl HooksFormat for CursorFormat {
    fn ensure_root(&self, cfg: &mut Map<String, Value>) {
        cfg.entry("version").or_insert(json!(1));
    }

    fn upsert(&self, hooks: &mut Map<String, Value>, event: &str, cmd: &str) -> Upsert {
        let arr = hooks.entry(event).or_insert_with(|| Value::Array(vec![]));
        if !arr.is_array() {
            *arr = Value::Array(vec![]);
        }
        let arr = arr.as_array_mut().expect("array");
        for h in arr.iter_mut() {
            if h.get("command").and_then(Value::as_str).map(super::is_owned_command).unwrap_or(false) {
                if h["command"] == cmd {
                    return Upsert::Unchanged;
                }
                h["command"] = Value::String(cmd.to_string());
                return Upsert::Updated;
            }
        }
        arr.push(json!({"command": cmd, "timeout": 5}));
        Upsert::Added
    }

    fn remove_owned(&self, arr: &mut Vec<Value>) -> usize {
        let before = arr.len();
        arr.retain(|h| !h.get("command").and_then(Value::as_str).map(super::is_owned_command).unwrap_or(false));
        before - arr.len()
    }

    fn commands(&self, arr: &[Value]) -> Vec<String> {
        arr.iter().filter_map(|h| h.get("command").and_then(Value::as_str).map(str::to_string)).collect()
    }
}

pub struct CursorConnector {
    pub hooks_path: PathBuf,
    pub state_dir: PathBuf,
}

impl CursorConnector {
    pub fn for_user() -> Self {
        Self { hooks_path: crate::user_home().join(".cursor").join("hooks.json"), state_dir: crate::agenttracker_home().join("connectors") }
    }

    pub fn with_paths(hooks_path: PathBuf, state_dir: PathBuf) -> Self {
        Self { hooks_path, state_dir }
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir.join("cursor_install.json")
    }

    pub fn detect(&self) -> DetectResult {
        let version = probe_version(&["cursor", "cursor.cmd", "cursor.exe"], Duration::from_secs(4));
        let cursor_dir = self.hooks_path.parent().map(Path::exists).unwrap_or(false);
        let mut notes = vec!["Experimental connector: verified with config files only, not a live Cursor.".to_string()];
        if !cursor_dir {
            notes.push("No ~/.cursor folder found — Cursor may not be installed for this user.".into());
        }
        let hooks_installed = read_json_file(&self.hooks_path).ok().flatten().map(|c| !json_hooks::owned_events(&CursorFormat, &c).is_empty()).unwrap_or(false);
        DetectResult {
            connector: NAME.into(),
            agent_found: version.is_some() || cursor_dir,
            agent_version: version,
            config_path: self.hooks_path.to_string_lossy().into_owned(),
            config_exists: self.hooks_path.exists(),
            hooks_installed,
            notes,
        }
    }

    pub fn preview(&self, hook_binary_path: &Path) -> Result<super::ConfigPreview> {
        json_hooks::preview(&CursorFormat, NAME, &self.hooks_path, HOOK_EVENTS, hook_binary_path)
    }

    pub fn install(&self, hook_binary_path: &Path) -> Result<InstallReport> {
        json_hooks::install(&CursorFormat, NAME, &self.hooks_path, &self.state_file(), HOOK_EVENTS, hook_binary_path)
    }

    pub fn uninstall(&self) -> Result<UninstallReport> {
        json_hooks::uninstall(&CursorFormat, &self.hooks_path, &self.state_file())
    }

    pub fn connection_test(&self) -> ConnectionTest {
        let mut steps = vec![];
        let cfg = match read_json_file(&self.hooks_path) {
            Ok(Some(c)) => {
                let v1 = c.get("version").and_then(Value::as_i64) == Some(1);
                steps.push(TestStep {
                    name: "hooks.json".into(),
                    ok: v1,
                    detail: if v1 { "valid, version 1".into() } else { "missing \"version\": 1".into() },
                });
                Some(c)
            }
            Ok(None) => {
                steps.push(TestStep { name: "hooks.json".into(), ok: false, detail: format!("{} doesn't exist", self.hooks_path.display()) });
                None
            }
            Err(e) => {
                steps.push(TestStep { name: "hooks.json".into(), ok: false, detail: e.to_string() });
                None
            }
        };
        let owned = cfg.as_ref().map(|c| json_hooks::owned_events(&CursorFormat, c)).unwrap_or_default();
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
            c(EventType::SessionStarted, true, "sessionStart hook"),
            c(EventType::TaskSubmitted, false, "beforeSubmitPrompt can block prompts; AgentTracker doesn't register blocking hooks"),
            c(EventType::ToolStarted, false, "preToolUse can block tools; AgentTracker doesn't register blocking hooks"),
            c(EventType::ToolFinished, true, "postToolUse / afterShellExecution / afterMCPExecution / afterFileEdit"),
            c(EventType::AgentReportedCompletion, true, "stop hook"),
            c(EventType::SessionEnded, true, "sessionEnd hook"),
            c(EventType::CaptureError, true, "Generated by AgentTracker"),
        ]
    }
}

pub fn normalize(env: &HookEnvelope) -> Option<AgentEvent> {
    let p = &env.payload;
    let name = p.get("hook_event_name").and_then(Value::as_str).unwrap_or(&env.hook_event);
    let t = match name {
        "sessionStart" => EventType::SessionStarted,
        "beforeSubmitPrompt" => EventType::TaskSubmitted,
        "preToolUse" | "beforeShellExecution" | "beforeMCPExecution" => EventType::ToolStarted,
        "postToolUse" | "postToolUseFailure" | "afterShellExecution" | "afterMCPExecution" | "afterFileEdit" => EventType::ToolFinished,
        "stop" => EventType::AgentReportedCompletion,
        "sessionEnd" => EventType::SessionEnded,
        _ => EventType::CaptureError,
    };
    let mut ev = base_event(env, t);
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    ev.session_id = s("conversation_id").or_else(|| s("session_id"));
    ev.agent_version = s("cursor_version");
    ev.working_dir = p.get("workspace_roots").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string).or_else(|| s("cwd"));
    ev.tool_call_id = s("tool_use_id").or_else(|| s("generation_id"));
    let mut payload = p.clone();
    if let Some(o) = payload.as_object_mut() {
        // Drop identifying/bulky fields we don't need to keep.
        o.remove("user_email");
        o.remove("transcript_path");
        o.remove("model_params");
    }
    ev.payload_redacted = if t == EventType::CaptureError { json!({"error": format!("unrecognized Cursor hook \"{name}\""), "payload": payload}) } else { json!({"hook": name, "data": payload}) };
    ev.redact();
    Some(ev)
}
