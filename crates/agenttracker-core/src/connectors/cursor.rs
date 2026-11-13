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
//! Mostly observational hooks. The exception is the Houndbit Protect folder guard: it registers
//! `preToolUse`, `beforeShellExecution` and `beforeMCPExecution`, which can answer
//! `{"permission":"deny"}`. The hook program always answers these with JSON (`allow` unless a
//! guarded folder is involved), because Cursor blocks the action when a permission hook prints
//! nothing. These get a long timeout so an "Ask me first" folder can wait for the owner.
//! Not tried against a live Cursor yet.
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

pub const HOOK_EVENTS: &[&str] = &["sessionStart", "preToolUse", "beforeShellExecution", "beforeMCPExecution", "postToolUse", "postToolUseFailure", "afterShellExecution", "afterMCPExecution", "afterFileEdit", "stop", "sessionEnd"];

/// Hooks the folder guard answers through. They may wait for the owner (up to 90 s in the hook).
pub const GUARD_EVENTS: &[&str] = &["preToolUse", "beforeShellExecution", "beforeMCPExecution"];
pub const GUARD_TIMEOUT_SECS: u64 = 120;

fn timeout_for(event: &str) -> u64 {
    if GUARD_EVENTS.contains(&event) {
        GUARD_TIMEOUT_SECS
    } else {
        5
    }
}

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
                if h["command"] == cmd && h.get("timeout").and_then(Value::as_u64) == Some(timeout_for(event)) {
                    return Upsert::Unchanged;
                }
                h["command"] = Value::String(cmd.to_string());
                h["timeout"] = json!(timeout_for(event));
                return Upsert::Updated;
            }
        }
        arr.push(json!({"command": cmd, "timeout": timeout_for(event)}));
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

    /// The hook program our installed hooks point at, if any are installed.
    pub fn installed_hook_path(&self) -> Option<std::path::PathBuf> {
        let cfg = read_json_file(&self.hooks_path).ok().flatten()?;
        json_hooks::owned_events(&CursorFormat, &cfg).first().and_then(|(_, c)| hook_path_from_command(c))
    }

    /// Whether the folder guard's hooks are installed with the long timeout they need. `None`
    /// when Houndbit's Cursor hooks aren't installed at all.
    pub fn guard_ready(&self) -> Option<bool> {
        let cfg = read_json_file(&self.hooks_path).ok().flatten()?;
        let hooks = cfg.get("hooks")?.as_object()?;
        let owned = |ev: &str| -> Vec<Value> {
            hooks.get(ev).and_then(Value::as_array).map(|a| a.iter().filter(|h| h.get("command").and_then(Value::as_str).map(super::is_owned_command).unwrap_or(false)).cloned().collect()).unwrap_or_default()
        };
        if HOOK_EVENTS.iter().all(|e| owned(e).is_empty()) {
            return None;
        }
        Some(GUARD_EVENTS.iter().all(|e| {
            let o = owned(e);
            !o.is_empty() && o.iter().all(|h| h.get("timeout").and_then(Value::as_u64).unwrap_or(0) >= GUARD_TIMEOUT_SECS)
        }))
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
            name: "Houndbit hooks".into(),
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
            c(EventType::TaskSubmitted, false, "beforeSubmitPrompt can block prompts; Houndbit doesn't register blocking hooks"),
            c(EventType::ToolStarted, true, "preToolUse / beforeShellExecution / beforeMCPExecution (also used by the Protect folder guard)"),
            c(EventType::ToolFinished, true, "postToolUse / afterShellExecution / afterMCPExecution / afterFileEdit"),
            c(EventType::AgentReportedCompletion, true, "stop hook"),
            c(EventType::SessionEnded, true, "sessionEnd hook"),
            c(EventType::CaptureError, true, "Generated by Houndbit"),
        ]
    }
}

/// Cursor on Windows sends folders as "/c:/Users/Greg/x" (and sometimes "file:///c%3A/..."). Turns
/// those into "c:/Users/Greg/x" so the folder can be read; other paths are returned unchanged.
pub fn windows_path(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if let Some(rest) = s.strip_prefix("file://") {
        s = rest.replace("%3A", ":").replace("%3a", ":").replace("%20", " ");
    }
    let b = s.as_bytes();
    if b.len() >= 3 && (b[0] == b'/' || b[0] == b'\\') && b[1].is_ascii_alphabetic() && b[2] == b':' && (b.len() == 3 || b[3] == b'/' || b[3] == b'\\') {
        s.remove(0);
    }
    s
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
    ev.working_dir = p.get("workspace_roots").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string).or_else(|| s("cwd")).map(|w| windows_path(&w));
    ev.tool_call_id = s("tool_use_id").or_else(|| s("generation_id"));
    let mut payload = p.clone();
    if let Some(o) = payload.as_object_mut() {
        if let Some(f) = o.get("file_path").and_then(Value::as_str).map(windows_path) {
            o.insert("file_path".into(), Value::String(f));
        }
        if let Some(Value::Array(roots)) = o.get_mut("workspace_roots") {
            for r in roots.iter_mut() {
                if let Some(x) = r.as_str().map(windows_path) {
                    *r = Value::String(x);
                }
            }
        }
        // Drop identifying/bulky fields we don't need to keep.
        o.remove("user_email");
        o.remove("transcript_path");
        o.remove("model_params");
    }
    ev.payload_redacted = if t == EventType::CaptureError { json!({"error": format!("unrecognized Cursor hook \"{name}\""), "payload": payload}) } else { json!({"hook": name, "data": payload}) };
    ev.redact();
    Some(ev)
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn env(connector: &str, event: &str, payload: Value) -> HookEnvelope {
        serde_json::from_value(json!({"connector": connector, "hook_event": event, "hook_ts_ns": 1, "payload": payload})).unwrap()
    }

    #[test]
    fn windows_paths_from_cursor_are_fixed() {
        assert_eq!(windows_path("/c:/Users/Greg/.agents"), "c:/Users/Greg/.agents");
        assert_eq!(windows_path("file:///c%3A/Users/Greg/My%20Stuff"), "c:/Users/Greg/My Stuff");
        assert_eq!(windows_path("/c:"), "c:");
        assert_eq!(windows_path("C:\\Users\\Greg"), "C:\\Users\\Greg");
        assert_eq!(windows_path("/home/greg/x"), "/home/greg/x");
        assert_eq!(windows_path("/ab/c"), "/ab/c");
    }

    #[test]
    fn install_adds_guard_hooks_with_a_long_timeout_and_upgrades_old_installs() {
        let t = tempfile::tempdir().unwrap();
        let hooks = t.path().join("hooks.json");
        let hook = t.path().join("agenttracker-hook.exe");
        std::fs::write(&hook, b"").unwrap();
        let c = CursorConnector::with_paths(hooks.clone(), t.path().join("state"));
        assert_eq!(c.guard_ready(), None);
        c.install(&hook).unwrap();
        assert_eq!(c.guard_ready(), Some(true));
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
        for ev in GUARD_EVENTS {
            assert_eq!(cfg["hooks"][ev][0]["timeout"], 120, "{ev}");
        }
        assert_eq!(cfg["hooks"]["afterFileEdit"][0]["timeout"], 5);

        // An older install had only the report hooks.
        let mut old = cfg.clone();
        for ev in GUARD_EVENTS {
            old["hooks"].as_object_mut().unwrap().remove(*ev);
        }
        std::fs::write(&hooks, old.to_string()).unwrap();
        assert_eq!(c.guard_ready(), Some(false));
        c.install(&hook).unwrap();
        assert_eq!(c.guard_ready(), Some(true));
        let mut short = cfg.clone();
        short["hooks"]["preToolUse"][0]["timeout"] = json!(5);
        std::fs::write(&hooks, short.to_string()).unwrap();
        assert_eq!(c.guard_ready(), Some(false));
        c.install(&hook).unwrap();
        assert_eq!(c.guard_ready(), Some(true));
    }

    #[test]
    fn cursor_event_gets_a_readable_folder_and_file() {
        let e = env("cursor", "afterFileEdit", json!({"hook_event_name": "afterFileEdit", "cursor_version": "2.0", "conversation_id": "c1", "workspace_roots": ["/c:/Users/Greg/.agents"], "file_path": "/c:/Users/Greg/.agents/test86.txt"}));
        let ev = normalize(&e).unwrap();
        assert_eq!(ev.working_dir.as_deref(), Some("c:/Users/Greg/.agents"));
        assert_eq!(ev.payload_redacted.pointer("/data/file_path").and_then(Value::as_str), Some("c:/Users/Greg/.agents/test86.txt"));
    }

    #[test]
    fn claude_hooks_run_by_cursor_are_skipped() {
        let e = env("claude_code", "PostToolUse", json!({"hook_event_name": "postToolUse", "cursor_version": "2.0", "conversation_id": "c1"}));
        assert!(crate::connectors::claude_code::normalize(&e).is_none());
        let e = env("claude_code", "Stop", json!({"hook_event_name": "stop"}));
        assert!(crate::connectors::claude_code::normalize(&e).is_none());
        let real = env("claude_code", "Stop", json!({"hook_event_name": "Stop", "session_id": "s1", "cwd": "C:/x"}));
        assert!(crate::connectors::claude_code::normalize(&real).is_some());
    }
}
