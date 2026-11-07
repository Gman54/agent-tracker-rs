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
pub const HOOK_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Notification", "Stop", "SessionEnd"];
const TOOL_EVENTS: &[&str] = &["PreToolUse", "PostToolUse", "PostToolUseFailure"];

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

/// Looks in the documented / common install locations for Claude Code.
/// Returns (stand-alone CLI executables, apps that bundle Claude Code). Pure apart from
/// `exists()` checks so it can be tested with a fake home folder.
pub(crate) fn find_claude_installs(home: &Path, appdata: Option<PathBuf>, localappdata: Option<PathBuf>) -> (Vec<PathBuf>, Vec<(&'static str, PathBuf)>) {
    let mut cli = vec![
        // Native installer (Windows: %USERPROFILE%\.local\bin\claude.exe; macOS/Linux: ~/.local/bin/claude).
        home.join(".local").join("bin").join("claude.exe"),
        home.join(".local").join("bin").join("claude"),
        home.join(".claude").join("local").join("claude.exe"),
        home.join(".claude").join("local").join("claude"),
    ];
    if let Some(a) = &appdata {
        cli.push(a.join("npm").join("claude.cmd"));
        cli.push(a.join("npm").join("claude.exe"));
    }
    if let Some(l) = &localappdata {
        cli.push(l.join("Microsoft").join("WinGet").join("Links").join("claude.exe"));
    }
    let mut cli: Vec<PathBuf> = cli.into_iter().filter(|p| p.is_file()).collect();

    // Claude desktop app (Code tab) keeps its own copy of Claude Code:
    //   MSIX:     %LOCALAPPDATA%\Packages\Claude_<id>\LocalCache\Roaming\Claude\claude-code\<version>\claude.exe
    //   Non-MSIX: %APPDATA%\Claude\claude-code\<version>\claude.exe
    let packages = msix_claude_packages(localappdata.as_deref());
    let mut desktop_roots: Vec<PathBuf> = packages.iter().map(|p| p.join("LocalCache").join("Roaming").join("Claude").join("claude-code")).collect();
    if let Some(a) = &appdata {
        desktop_roots.push(a.join("Claude").join("claude-code"));
    }
    for root in &desktop_roots {
        cli.extend(desktop_cli_versions(root));
    }

    let mut bundled = vec![];
    for p in packages {
        bundled.push(("Claude desktop app", p));
    }
    if let Some(a) = &appdata {
        let p = a.join("Claude");
        if p.join("claude-code").is_dir() {
            bundled.push(("Claude desktop app", p));
        }
    }
    if let Some(l) = &localappdata {
        let p = l.join("Claude");
        if p.is_dir() {
            bundled.push(("Claude desktop app", p));
        }
        let p = l.join("AnthropicClaude");
        if p.is_dir() {
            bundled.push(("Claude desktop app", p));
        }
        let p = l.join("Programs").join("Claude");
        if p.is_dir() {
            bundled.push(("Claude desktop app", p));
        }
    }
    for ext_root in [home.join(".vscode").join("extensions"), home.join(".cursor").join("extensions")] {
        if let Ok(rd) = std::fs::read_dir(&ext_root) {
            if let Some(e) = rd.flatten().find(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("anthropic.claude-code")) {
                bundled.push(("Claude Code editor extension", e.path()));
            }
        }
    }
    (cli, bundled)
}

/// `%LOCALAPPDATA%\Packages\Claude_*` folders (the Microsoft Store / MSIX desktop app), sorted by name.
fn msix_claude_packages(localappdata: Option<&Path>) -> Vec<PathBuf> {
    let Some(l) = localappdata else { return vec![] };
    let Ok(rd) = std::fs::read_dir(l.join("Packages")) else { return vec![] };
    let mut v: Vec<PathBuf> = rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("Claude_") && e.path().is_dir()).map(|e| e.path()).collect();
    v.sort();
    v
}

/// `<root>\<version>\claude.exe` for every version folder, newest version first.
fn desktop_cli_versions(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else { return vec![] };
    let mut found: Vec<(Vec<u64>, String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let exe = e.path().join("claude.exe");
            exe.is_file().then(|| {
                let name = e.file_name().to_string_lossy().into_owned();
                (version_key(&name), name, exe)
            })
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, p)| p).collect()
}

/// Numeric parts of a version folder name ("2.1.10" -> [2, 1, 10]) so 2.1.10 sorts above 2.1.9.
fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0)).collect()
}

/// True for a claude.exe that ships inside the Claude desktop app (lives under a `Claude\claude-code` folder).
fn is_desktop_bundled(p: &Path) -> bool {
    let parts: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase()).collect();
    parts.windows(2).any(|w| w[0] == "claude" && w[1] == "claude-code")
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
        let config_exists = self.settings_path.exists();
        let mut notes = vec![];
        let mut version = probe_version(&["claude", "claude.cmd", "claude.exe"], Duration::from_secs(4));
        if version.is_none() {
            // Not on PATH (common when the installer updated PATH after this app's parent
            // process started, or when Claude Code only ships inside another app).
            let (cli, bundled) = find_claude_installs(&crate::user_home(), std::env::var_os("APPDATA").map(PathBuf::from), std::env::var_os("LOCALAPPDATA").map(PathBuf::from));
            for p in &cli {
                if let Some(v) = probe_version(&[p.to_string_lossy().as_ref()], Duration::from_secs(4)) {
                    if is_desktop_bundled(p) {
                        notes.push(format!("Found the Claude Code that comes with the Claude desktop app, at {}.", p.display()));
                    } else {
                        notes.push(format!("Claude Code found at {} (it isn't on the PATH this app sees).", p.display()));
                    }
                    version = Some(v);
                    break;
                }
            }
            if version.is_none() {
                if let Some(p) = cli.first() {
                    if is_desktop_bundled(p) {
                        notes.push(format!("Found the Claude Code that comes with the Claude desktop app, at {}. It didn't tell us its version, which is normal for the built-in copy.", p.display()));
                        version = Some("Claude desktop app (built-in Claude Code)".into());
                    } else {
                        notes.push(format!("Found {} but it didn't answer `--version`.", p.display()));
                        version = Some("Found (version unknown)".into());
                    }
                }
            }
            if version.is_none() {
                if let Some((label, p)) = bundled.first() {
                    notes.push(format!("{label} found at {}. It has Claude Code built in. If nothing shows up after you start a Claude session, try running `claude` from a terminal instead.", p.display()));
                    version = Some(format!("{label} (built-in Claude Code)"));
                }
            }
        }
        if !config_exists {
            for alt in Self::alternate_paths() {
                if alt.exists() {
                    notes.push(format!("Found a Claude settings file at {} (AgentTracker uses {})", alt.display(), self.settings_path.display()));
                }
            }
        }
        if version.is_none() {
            notes.push("We couldn't spot Claude Code in the usual places, but that's okay. The hooks are ready and will start working as soon as Claude Code runs. Try starting a Claude session (for example in the Code tab of the Claude desktop app), then check back here.".into());
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

    pub fn preview(&self, hook_binary_path: &Path) -> Result<super::ConfigPreview> {
        json_hooks::preview(&ClaudeFormat, NAME, &self.settings_path, HOOK_EVENTS, hook_binary_path)
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
    // Where Claude Code keeps its own record of this conversation (read later for the report's
    // "Conversation" section). Only plausible paths inside a Claude folder are kept.
    if let (Some(tp), Some(obj)) = (p.get("transcript_path").and_then(Value::as_str).and_then(crate::conversation::plausible_transcript_path), ev.payload_redacted.as_object_mut()) {
        obj.insert("transcript_path".into(), Value::String(tp));
    }
    ev.redact();
    Some(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_claude_outside_path() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let local = t.path().join("local");
        let (cli, bundled) = find_claude_installs(&home, None, Some(local.clone()));
        assert!(cli.is_empty() && bundled.is_empty());

        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::write(home.join(".local/bin/claude.exe"), b"").unwrap();
        std::fs::create_dir_all(local.join("AnthropicClaude")).unwrap();
        std::fs::create_dir_all(home.join(".vscode/extensions/anthropic.claude-code-2.1.0")).unwrap();
        let (cli, bundled) = find_claude_installs(&home, None, Some(local));
        assert_eq!(cli, vec![home.join(".local/bin/claude.exe")]);
        assert_eq!(bundled.len(), 2);
        assert_eq!(bundled[0].0, "Claude desktop app");
        assert_eq!(bundled[1].0, "Claude Code editor extension");
    }

    fn fake_exe(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let exe = dir.join("claude.exe");
        std::fs::write(&exe, b"").unwrap();
        exe
    }

    #[test]
    fn finds_msix_desktop_app_newest_version_first() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let local = t.path().join("local");
        let pkg = local.join("Packages").join("Claude_pzs8sxrjxfjjc");
        let cc = pkg.join("LocalCache").join("Roaming").join("Claude").join("claude-code");
        let older = fake_exe(&cc.join("2.1.9"));
        let newer = fake_exe(&cc.join("2.1.10"));
        // A folder without claude.exe and an unrelated package are ignored.
        std::fs::create_dir_all(cc.join("2.2.0")).unwrap();
        std::fs::create_dir_all(local.join("Packages").join("Microsoft.Something_abc")).unwrap();

        let (cli, bundled) = find_claude_installs(&home, None, Some(local));
        assert_eq!(cli, vec![newer.clone(), older]);
        assert!(is_desktop_bundled(&newer));
        assert_eq!(bundled, vec![("Claude desktop app", pkg)]);
    }

    #[test]
    fn finds_appdata_desktop_app_layout() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let appdata = t.path().join("roaming");
        let local = t.path().join("local");
        let exe = fake_exe(&appdata.join("Claude").join("claude-code").join("1.0.5"));
        std::fs::create_dir_all(local.join("Claude").join("logs")).unwrap();

        let (cli, bundled) = find_claude_installs(&home, Some(appdata.clone()), Some(local.clone()));
        assert_eq!(cli, vec![exe.clone()]);
        assert!(is_desktop_bundled(&exe));
        assert!(!is_desktop_bundled(&home.join(".local/bin/claude.exe")));
        assert_eq!(bundled, vec![("Claude desktop app", appdata.join("Claude")), ("Claude desktop app", local.join("Claude"))]);
    }

    #[test]
    fn version_folders_sort_numerically() {
        assert!(version_key("2.1.10") > version_key("2.1.9"));
        assert!(version_key("10.0.0") > version_key("9.9.9"));
    }
}
