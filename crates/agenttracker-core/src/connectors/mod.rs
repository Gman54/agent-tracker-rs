//! Agent connectors: detect an agent, install/uninstall AgentTracker's hooks into its config
//! (merging, backing up, never clobbering unrelated settings), test the connection, and normalize
//! hook payloads into [`AgentEvent`]s.
pub mod claude_code;
pub mod cursor;
pub(crate) mod json_hooks;
pub mod manual;

use crate::event::{AgentEvent, EventType};
use crate::{CoreError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Every hook command AgentTracker writes contains this marker; uninstall removes only entries
/// carrying it.
pub const OWNER_MARKER: &str = "--at-owner agenttracker";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Maturity {
    /// Implemented against documented APIs; needs a live agent for full validation.
    Implemented,
    /// API not fully validated; only parts are tested.
    Experimental,
    /// No agent integration — file verification only.
    FileOnly,
}

/// What a hook delivers to the collector (written by agenttracker-hook).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEnvelope {
    pub connector: String,
    /// Native hook event name (e.g. "PreToolUse", "afterFileEdit").
    pub hook_event: String,
    /// Wall-clock nanoseconds when the hook ran — used as the source ordering key.
    pub hook_ts_ns: i64,
    #[serde(default)]
    pub hook_version: String,
    /// The agent's stdin JSON (or `{}` when none).
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectResult {
    pub connector: String,
    /// Agent binary answered `--version`.
    pub agent_found: bool,
    pub agent_version: Option<String>,
    /// Config file AgentTracker would edit.
    pub config_path: String,
    pub config_exists: bool,
    pub hooks_installed: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub event_type: EventType,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReport {
    pub config_path: String,
    pub backup_path: Option<String>,
    pub created_file: bool,
    pub added: Vec<String>,
    pub updated: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallReport {
    pub config_path: String,
    pub removed: Vec<String>,
    /// "restored_backup" | "deleted_created_file" | "surgical_removal" | "nothing_to_do"
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStep {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTest {
    pub ok: bool,
    pub steps: Vec<TestStep>,
}

/// Record of what an install wrote, kept in the AgentTracker state dir.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallState {
    pub config_path: String,
    pub backup_path: Option<String>,
    pub created_file: bool,
    pub hooks_key_existed: bool,
    pub written_sha256: String,
    pub added: Vec<String>,
    pub installed_at: i64,
}

/// Metadata shared by all connectors, used by the UI/registry.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorInfo {
    pub name: &'static str,
    pub display_name: &'static str,
    pub maturity: Maturity,
    pub status_note: &'static str,
    pub docs_checked: &'static str,
}

/// Builds the hook command line. Forward slashes work in cmd, PowerShell and bash on Windows.
pub fn hook_command(hook_binary: &Path, connector: &str, event: &str) -> String {
    let p = hook_binary.to_string_lossy().replace('\\', "/");
    format!("\"{p}\" --connector {connector} --event {event} {OWNER_MARKER}")
}

pub fn is_owned_command(cmd: &str) -> bool {
    cmd.contains(OWNER_MARKER)
}

/// Extracts the hook binary path from one of our commands.
pub fn hook_path_from_command(cmd: &str) -> Option<PathBuf> {
    let rest = cmd.strip_prefix('"')?;
    rest.find('"').map(|end| PathBuf::from(&rest[..end]))
}

pub(crate) fn read_json_file(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(Some(Value::Object(Default::default()))),
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| CoreError::Invalid(format!("{} is not valid JSON ({e}); AgentTracker won't modify it", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes atomically (temp file + rename) and returns the SHA-256 of what was written.
pub(crate) fn write_json_file(path: &Path, v: &Value) -> Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(v)? + "\n";
    let tmp = path.with_extension("agenttracker-tmp");
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, path)?;
    Ok(tracker_common::security::sha256_hex(text.as_bytes()))
}

pub(crate) fn file_sha(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| tracker_common::security::sha256_hex(&b))
}

pub(crate) fn load_state(state_file: &Path) -> Option<InstallState> {
    std::fs::read_to_string(state_file).ok().and_then(|s| serde_json::from_str(&s).ok())
}

pub(crate) fn save_state(state_file: &Path, st: &InstallState) -> Result<()> {
    if let Some(parent) = state_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(state_file, serde_json::to_string_pretty(st)?)?;
    Ok(())
}

/// Runs `<bin> --version` with a timeout; returns the first non-empty output line.
pub fn probe_version(candidates: &[&str], timeout: Duration) -> Option<String> {
    for c in candidates {
        let Ok(mut child) = Command::new(c).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
            continue;
        };
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let out = child.wait_with_output().ok()?;
                    let s = String::from_utf8_lossy(&out.stdout);
                    if let Some(line) = s.lines().map(str::trim).find(|l| !l.is_empty()) {
                        return Some(line.chars().take(120).collect());
                    }
                    break;
                }
                Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = child.kill();
                    break;
                }
            }
        }
    }
    None
}

/// Common connection-test step: the hook binary exists and answers `--version`.
pub(crate) fn hook_binary_step(path: Option<PathBuf>) -> TestStep {
    match path {
        None => TestStep { name: "Hook program".into(), ok: false, detail: "No AgentTracker hook is configured".into() },
        Some(p) if !p.exists() => TestStep { name: "Hook program".into(), ok: false, detail: format!("{} doesn't exist", p.display()) },
        Some(p) => match probe_version(&[p.to_string_lossy().as_ref()], Duration::from_secs(3)) {
            Some(v) => TestStep { name: "Hook program".into(), ok: true, detail: format!("{v} at {}", p.display()) },
            None => TestStep { name: "Hook program".into(), ok: false, detail: format!("{} didn't respond", p.display()) },
        },
    }
}

/// Normalizes a delivery for whichever connector produced it.
pub fn normalize(env: &HookEnvelope) -> Option<AgentEvent> {
    match env.connector.as_str() {
        claude_code::NAME => claude_code::normalize(env),
        cursor::NAME => cursor::normalize(env),
        _ => {
            let mut ev = base_event(env, EventType::CaptureError);
            ev.payload_redacted = serde_json::json!({"error": format!("unknown connector \"{}\"", env.connector), "hook_event": env.hook_event});
            Some(ev)
        }
    }
}

/// Fields common to every hook-derived event. The id is derived from the envelope content so
/// re-delivery (e.g. fallback replay) deduplicates.
pub(crate) fn base_event(env: &HookEnvelope, t: EventType) -> AgentEvent {
    let id = crate::event::content_event_id(&env.connector, &serde_json::to_value(env).unwrap_or(Value::Null));
    let mut ev = AgentEvent::new(&env.connector, crate::VERSION, t, crate::event::CaptureSource::Hook, Some(id));
    // The id was derived by the collector (deterministically), not supplied by the agent.
    ev.collector_generated = true;
    ev.source_seq = Some(env.hook_ts_ns);
    ev.source_ts = Some(env.hook_ts_ns / 1_000_000);
    ev.raw_source = Some(serde_json::json!({"hook_event": env.hook_event, "hook_version": env.hook_version}));
    ev
}

pub fn registry() -> Vec<ConnectorInfo> {
    vec![claude_code::info(), cursor::info(), manual::info()]
}
