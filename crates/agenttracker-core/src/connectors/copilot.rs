//! GitHub Copilot agent in VS Code connector.
//!
//! Status: **Experimental — not live-tested.** Tested with fake payloads and temp config files only.
//! Docs checked 2026-11-03 (neither page publishes a minimum version; VS Code hooks are "Preview"):
//! - https://code.visualstudio.com/docs/copilot/customization/hooks
//! - https://docs.github.com/en/copilot/reference/hooks-reference
//!
//! VS Code's agent uses the shared Copilot harness, which loads user hook files from
//! `~/.copilot/hooks/*.json` (`$COPILOT_HOME/hooks/` when `COPILOT_HOME` is set). TrailFox
//! writes its own file there, `agenttracker.json`, in the documented format
//! `{"version": 1, "hooks": {"<event>": [{"type": "command", "bash": "...", "powershell": "...", "timeoutSec": 5}]}}`.
//!
//! Only observational events are registered (sessionStart, userPromptSubmitted, postToolUse,
//! agentStop, sessionEnd). `preToolUse` and `permissionRequest` can deny tool calls and are
//! fail-closed when a hook crashes, so TrailFox never registers them.
//! Payloads (camelCase): every event carries `sessionId`, `timestamp` (ms) and `cwd`.
//! PascalCase aliases select a snake_case payload (`session_id`, `hook_event_name`, ...); both
//! shapes are normalized. A payload without a session id or timestamp is treated as an
//! unsupported version.
use super::json_hooks::{self, HooksFormat, Upsert};
use super::{
    base_event, hook_binary_step, hook_path_from_command, probe_version, read_json_file, Capability, ConfigPreview, ConnectionTest, ConnectorInfo, DetectResult,
    HookEnvelope, InstallReport, Maturity, TestStep, UninstallReport, UNSUPPORTED_MARKER,
};
use crate::event::{AgentEvent, EventType};
use crate::Result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "copilot_vscode";

pub const HOOK_EVENTS: &[&str] = &["sessionStart", "userPromptSubmitted", "postToolUse", "agentStop", "sessionEnd"];

pub const DOCS: &[&str] = &["https://code.visualstudio.com/docs/copilot/customization/hooks", "https://docs.github.com/en/copilot/reference/hooks-reference"];

pub fn info() -> ConnectorInfo {
    ConnectorInfo {
        name: NAME,
        display_name: "GitHub Copilot (VS Code agent)",
        maturity: Maturity::Experimental,
        status_note: "Experimental — not live-tested. Uses the documented Copilot hook files (~/.copilot/hooks). VS Code hooks are a Preview feature.",
        docs_checked: "https://code.visualstudio.com/docs/copilot/customization/hooks and https://docs.github.com/en/copilot/reference/hooks-reference (fetched 2026-11-03, no minimum version published)",
    }
}

/// Default user hooks directory per the docs.
pub fn default_hooks_dir() -> PathBuf {
    match std::env::var_os("COPILOT_HOME").filter(|v| !v.is_empty()) {
        Some(h) => PathBuf::from(h).join("hooks"),
        None => crate::user_home().join(".copilot").join("hooks"),
    }
}

struct CopilotFormat;

/// PowerShell can't run `"C:/x.exe" args` directly; it needs the call operator.
fn powershell_cmd(cmd: &str) -> String {
    format!("& {cmd}")
}

fn entry_commands(h: &Value) -> Vec<String> {
    ["command", "bash", "powershell"].iter().filter_map(|k| h.get(*k).and_then(Value::as_str).map(str::to_string)).collect()
}

fn entry_owned(h: &Value) -> bool {
    entry_commands(h).iter().any(|c| super::is_owned_command(c))
}

impl HooksFormat for CopilotFormat {
    fn ensure_root(&self, cfg: &mut Map<String, Value>) {
        cfg.entry("version").or_insert(json!(1));
    }

    fn upsert(&self, hooks: &mut Map<String, Value>, event: &str, cmd: &str) -> Upsert {
        let arr = hooks.entry(event).or_insert_with(|| Value::Array(vec![]));
        if !arr.is_array() {
            *arr = Value::Array(vec![]);
        }
        let arr = arr.as_array_mut().expect("array");
        let want = json!({"type": "command", "bash": cmd, "powershell": powershell_cmd(cmd), "timeoutSec": 5});
        for h in arr.iter_mut() {
            if entry_owned(h) {
                if *h == want {
                    return Upsert::Unchanged;
                }
                *h = want;
                return Upsert::Updated;
            }
        }
        arr.push(want);
        Upsert::Added
    }

    fn remove_owned(&self, arr: &mut Vec<Value>) -> usize {
        let before = arr.len();
        arr.retain(|h| !entry_owned(h));
        before - arr.len()
    }

    fn commands(&self, arr: &[Value]) -> Vec<String> {
        // One representative command per entry (bash/command form starts with the quoted path).
        arr.iter().filter_map(|h| h.get("bash").or_else(|| h.get("command")).and_then(Value::as_str).map(str::to_string)).collect()
    }
}

pub struct CopilotConnector {
    pub hooks_path: PathBuf,
    pub state_dir: PathBuf,
}

impl CopilotConnector {
    pub fn for_user() -> Self {
        Self { hooks_path: default_hooks_dir().join("agenttracker.json"), state_dir: crate::agenttracker_home().join("connectors") }
    }

    pub fn with_paths(hooks_path: PathBuf, state_dir: PathBuf) -> Self {
        Self { hooks_path, state_dir }
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir.join("copilot_vscode_install.json")
    }

    /// The `.copilot` folder (parent of `hooks/`).
    fn copilot_dir(&self) -> Option<&Path> {
        self.hooks_path.parent().and_then(Path::parent)
    }

    pub fn detect(&self) -> DetectResult {
        let version = probe_version(&["code", "code.cmd", "code-insiders", "code-insiders.cmd"], Duration::from_secs(4));
        let copilot_dir = self.copilot_dir().map(Path::exists).unwrap_or(false);
        let mut notes = vec!["Experimental connector — not live-tested. VS Code agent hooks are a Preview feature.".to_string()];
        if version.is_none() {
            notes.push("The `code` command didn't answer — VS Code may not be on PATH.".into());
        }
        if !copilot_dir {
            notes.push("No ~/.copilot folder found yet — it will be created on connect.".into());
        }
        notes.push("Workspace trust and organization policy can turn hooks off; TrailFox can't see that setting.".into());
        let hooks_installed = read_json_file(&self.hooks_path).ok().flatten().map(|c| !json_hooks::owned_events(&CopilotFormat, &c).is_empty()).unwrap_or(false);
        DetectResult {
            connector: NAME.into(),
            agent_found: version.is_some() || copilot_dir,
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
        json_hooks::owned_events(&CopilotFormat, &cfg).first().and_then(|(_, c)| hook_path_from_command(c))
    }

    pub fn preview(&self, hook_binary_path: &Path) -> Result<ConfigPreview> {
        json_hooks::preview(&CopilotFormat, NAME, &self.hooks_path, HOOK_EVENTS, hook_binary_path)
    }

    pub fn install(&self, hook_binary_path: &Path) -> Result<InstallReport> {
        json_hooks::install(&CopilotFormat, NAME, &self.hooks_path, &self.state_file(), HOOK_EVENTS, hook_binary_path)
    }

    pub fn uninstall(&self) -> Result<UninstallReport> {
        json_hooks::uninstall(&CopilotFormat, &self.hooks_path, &self.state_file())
    }

    pub fn connection_test(&self) -> ConnectionTest {
        let mut steps = vec![];
        let cfg = match read_json_file(&self.hooks_path) {
            Ok(Some(c)) => {
                let v1 = c.get("version").and_then(Value::as_i64) == Some(1);
                let disabled = c.get("disableAllHooks").and_then(Value::as_bool).unwrap_or(false);
                steps.push(TestStep {
                    name: "Hook file".into(),
                    ok: v1 && !disabled,
                    detail: if disabled { "disableAllHooks is true in this file".into() } else if v1 { "valid, version 1".into() } else { "missing \"version\": 1".into() },
                });
                Some(c)
            }
            Ok(None) => {
                steps.push(TestStep { name: "Hook file".into(), ok: false, detail: format!("{} doesn't exist", self.hooks_path.display()) });
                None
            }
            Err(e) => {
                steps.push(TestStep { name: "Hook file".into(), ok: false, detail: e.to_string() });
                None
            }
        };
        let owned = cfg.as_ref().map(|c| json_hooks::owned_events(&CopilotFormat, c)).unwrap_or_default();
        let missing: Vec<&str> = HOOK_EVENTS.iter().copied().filter(|e| !owned.iter().any(|(oe, _)| oe == e)).collect();
        steps.push(TestStep {
            name: "TrailFox hooks".into(),
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
            c(EventType::TaskSubmitted, true, "userPromptSubmitted hook (prompt text is redacted before storage)"),
            c(EventType::ToolStarted, false, "preToolUse can deny tools and is fail-closed if a hook crashes; TrailFox doesn't register decision hooks"),
            c(EventType::ToolFinished, true, "postToolUse hook"),
            c(EventType::AgentReportedCompletion, true, "agentStop hook — the agent *says* it is done; files are verified separately"),
            c(EventType::SessionEnded, true, "sessionEnd hook"),
            c(EventType::CaptureError, true, "Generated by TrailFox when a payload can't be understood"),
        ]
    }
}

/// Maps both camelCase event names and their documented PascalCase aliases.
fn event_type(name: &str) -> Option<EventType> {
    Some(match name {
        "sessionStart" | "SessionStart" => EventType::SessionStarted,
        "userPromptSubmitted" | "UserPromptSubmit" => EventType::TaskSubmitted,
        "preToolUse" | "PreToolUse" => EventType::ToolStarted,
        "postToolUse" | "PostToolUse" | "postToolUseFailure" | "PostToolUseFailure" => EventType::ToolFinished,
        "agentStop" | "Stop" => EventType::AgentReportedCompletion,
        "sessionEnd" | "SessionEnd" => EventType::SessionEnded,
        _ => return None,
    })
}

pub fn normalize(env: &HookEnvelope) -> Option<AgentEvent> {
    let p = &env.payload;
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    let snake = p.get("hook_event_name").is_some() || p.get("session_id").is_some();
    let name = s("hook_event_name").unwrap_or_else(|| env.hook_event.clone());
    let session = if snake { s("session_id") } else { s("sessionId") };
    let has_ts = p.get("timestamp").map(|t| t.is_number() || t.is_string()).unwrap_or(false);

    let mut payload = p.clone();
    if let Some(o) = payload.as_object_mut() {
        o.remove("transcriptPath");
        o.remove("transcript_path");
    }
    let Some(t) = event_type(&name) else {
        let mut ev = base_event(env, EventType::CaptureError);
        ev.session_id = session;
        ev.working_dir = s("cwd");
        ev.payload_redacted = json!({"error": format!("unrecognized Copilot hook \"{name}\""), "payload": payload});
        ev.redact();
        return Some(ev);
    };
    if session.is_none() || !has_ts {
        // Missing documented required fields → treat as an unsupported agent version.
        let mut ev = base_event(env, EventType::CaptureError);
        ev.working_dir = s("cwd");
        ev.payload_redacted = json!({
            "error": format!("Copilot \"{name}\" payload is missing sessionId/timestamp — unsupported or changed agent version"),
            UNSUPPORTED_MARKER: true,
            "payload": payload,
        });
        ev.redact();
        return Some(ev);
    }
    let mut ev = base_event(env, t);
    ev.session_id = session;
    ev.working_dir = s("cwd");
    if !snake {
        if let Some(ms) = p.get("timestamp").and_then(Value::as_i64) {
            ev.source_ts = Some(ms);
        }
    }
    let g = |camel: &str, sn: &str| if snake { p.get(sn).cloned() } else { p.get(camel).cloned() };
    let failed = name.eq_ignore_ascii_case("postToolUseFailure");
    ev.payload_redacted = match t {
        EventType::ToolStarted => json!({"tool": g("toolName", "tool_name"), "input": g("toolArgs", "tool_input")}),
        EventType::ToolFinished => json!({"tool": g("toolName", "tool_name"), "input": g("toolArgs", "tool_input"), "response": g("toolResult", "tool_result"), "failed": failed, "error": p.get("error")}),
        EventType::TaskSubmitted => json!({"prompt": p.get("prompt")}),
        EventType::SessionStarted => json!({"source": p.get("source")}),
        EventType::SessionEnded => json!({"reason": p.get("reason")}),
        EventType::AgentReportedCompletion => json!({"stop_reason": g("stopReason", "stop_reason")}),
        EventType::CaptureError => json!({}),
    };
    ev.redact();
    Some(ev)
}
