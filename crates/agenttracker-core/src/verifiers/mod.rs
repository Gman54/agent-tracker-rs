//! Verifier plugins. Each check inspects evidence (manifests, events, change hints) and returns
//! Passed / Failed / UnableToVerify / NotApplicable / InsufficientCoverage. A check must say
//! "unable to verify" whenever the evidence doesn't cover what it asserts — never guess "passed".
//! A check with nothing to look at is "not applicable" (never "All 0 ... passed"), and a check
//! whose evidence can't see part of what it claims (e.g. shell commands) is "insufficient coverage".
pub mod auto_checks;
pub mod changes_within_scope;
pub mod folder_structure_matches;
pub mod inputs_preserved_in_output;
pub mod originals_unchanged;
pub mod required_outputs_exist;

use crate::event::AgentEvent;
use crate::fs_observer::{normalize_path, relative_to, ChangeHint, Manifest, ManifestEntry};
use crate::task::{FolderRole, TaskDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    UnableToVerify,
    /// Nothing for this check to look at (e.g. the agent reported no file edits). Neutral.
    NotApplicable,
    /// The evidence can't see everything the check claims (e.g. shell commands ran). Neutral.
    InsufficientCoverage,
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::UnableToVerify => "unable_to_verify",
            Self::NotApplicable => "not_applicable",
            Self::InsufficientCoverage => "insufficient_coverage",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub check_id: String,
    pub check_type: String,
    pub check_version: String,
    pub status: CheckStatus,
    pub summary: String,
    pub evidence_refs: Vec<String>,
    pub limitations: Vec<String>,
    pub started_at: i64,
    pub finished_at: i64,
}

/// Everything a verifier may look at.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub task: Option<TaskDefinition>,
    pub baselines: Vec<Manifest>,
    pub verifications: Vec<Manifest>,
    pub events: Vec<AgentEvent>,
    pub change_hints: Vec<ChangeHint>,
}

pub trait Verifier: Send + Sync {
    fn check_type(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn required_evidence(&self) -> &[&'static str];
    fn execute(&self, params: &Value, evidence: &Evidence) -> CheckResult;
}

/// Accumulates findings and produces a CheckResult with the correct precedence:
/// any definite failure → Failed; otherwise any uncertainty → UnableToVerify; else Passed.
pub(crate) struct Findings {
    started_at: i64,
    pub failures: Vec<String>,
    pub uncertain: Vec<String>,
    /// Set when there was nothing to check, or the evidence can't see everything; used only
    /// when there are no failures or uncertainties.
    pub neutral: Option<(CheckStatus, String)>,
    pub evidence: Vec<String>,
    pub limitations: Vec<String>,
}

impl Findings {
    pub fn new() -> Self {
        Self { started_at: crate::now_ms(), failures: vec![], uncertain: vec![], neutral: None, evidence: vec![], limitations: vec![] }
    }

    pub fn finish(self, v: &dyn Verifier, pass_summary: String) -> CheckResult {
        let (status, summary) = if !self.failures.is_empty() {
            (CheckStatus::Failed, summarize(&self.failures))
        } else if !self.uncertain.is_empty() {
            (CheckStatus::UnableToVerify, summarize(&self.uncertain))
        } else if let Some((st, why)) = self.neutral.clone() {
            (st, why)
        } else {
            (CheckStatus::Passed, pass_summary)
        };
        let mut limitations = self.limitations;
        if status == CheckStatus::Failed && !self.uncertain.is_empty() {
            limitations.push(format!("Also could not check: {}", summarize(&self.uncertain)));
        }
        CheckResult {
            check_id: String::new(),
            check_type: v.check_type().to_string(),
            check_version: v.version().to_string(),
            status,
            summary,
            evidence_refs: self.evidence,
            limitations,
            started_at: self.started_at,
            finished_at: crate::now_ms(),
        }
    }
}

fn summarize(items: &[String]) -> String {
    const MAX: usize = 5;
    let mut s = items.iter().take(MAX).cloned().collect::<Vec<_>>().join("; ");
    if items.len() > MAX {
        s.push_str(&format!("; and {} more", items.len() - MAX));
    }
    s
}

/// Reads a string-array param (accepts a single string too).
pub(crate) fn str_list(params: &Value, key: &str) -> Vec<String> {
    match params.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).filter(|s| !s.trim().is_empty()).collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => vec![],
    }
}

impl Evidence {
    /// Resolves a user path: absolute paths are normalized; relative paths are joined to the task's
    /// first working folder.
    pub fn resolve(&self, p: &str) -> String {
        let path = Path::new(p);
        if path.is_absolute() || p.starts_with('/') || p.starts_with('\\') {
            return normalize_path(path);
        }
        let base = self.task.as_ref().and_then(|t| t.folders_with(FolderRole::Working).into_iter().next());
        match base {
            Some(b) => normalize_path(&Path::new(&b).join(p)),
            None => normalize_path(path),
        }
    }

    pub fn task_folders(&self, role: FolderRole) -> Vec<String> {
        self.task.as_ref().map(|t| t.folders_with(role)).unwrap_or_default()
    }
}

/// Finds the manifest with the deepest root containing `abs` and returns it plus the path
/// relative to that root.
pub(crate) fn locate<'a>(manifests: &'a [Manifest], abs: &str) -> Option<(&'a Manifest, String)> {
    manifests
        .iter()
        .filter(|m| m.root_ok)
        .filter_map(|m| relative_to(&m.root, abs).map(|r| (m, r)))
        .max_by_key(|(m, _)| m.root.len())
}

/// Entries at or below `prefix` ("" = everything).
pub(crate) fn entries_under<'a>(m: &'a Manifest, prefix: &'a str) -> impl Iterator<Item = &'a ManifestEntry> + 'a {
    m.entries.iter().filter(move |e| !e.rel_path.is_empty() && (prefix.is_empty() || e.rel_path == prefix || e.rel_path.starts_with(&format!("{prefix}/"))))
}

pub(crate) fn abs_of(m: &Manifest, rel: &str) -> String {
    if rel.is_empty() {
        m.root.clone()
    } else {
        format!("{}/{}", m.root.trim_end_matches('/'), rel)
    }
}

/// What a check can actually vouch for. Used to word the overall result honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// Protects what was already there or limits where changes go. Says nothing about the output.
    Safety,
    /// Confirms the output is there (and not empty). Says nothing about whether it's right.
    OutputExists,
    /// Looks at what the output contains or how it's laid out.
    OutputContent,
}

pub fn check_kind(check_type: &str) -> CheckKind {
    match check_type {
        "required_outputs_exist" => CheckKind::OutputExists,
        "inputs_preserved_in_output" | "folder_structure_matches" => CheckKind::OutputContent,
        // originals_unchanged, no_unexpected_deletions, changes_within_scope, edits_inside_folder,
        // and anything unknown (never let an unknown check claim to judge the output).
        _ => CheckKind::Safety,
    }
}

/// Tool names that run shell commands, lower-case (Claude Code, Copilot, Cursor and friends).
const SHELL_TOOLS: &[&str] = &["bash", "powershell", "shell", "sh", "cmd", "terminal", "run_terminal_cmd", "run_in_terminal", "execute_command", "run_command", "exec"];

fn event_tool(e: &AgentEvent) -> String {
    let p = &e.payload_redacted;
    if let Some(h) = p.get("hook").and_then(Value::as_str) {
        if h.contains("ShellExecution") {
            return "shell".into();
        }
    }
    p.get("tool").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase()
}

fn call_key(e: &AgentEvent) -> String {
    e.tool_call_id.clone().unwrap_or_else(|| e.event_id.clone())
}

/// How many separate shell commands the agent ran, counted from start and finish events
/// (one command with both a start and a finish counts once).
pub fn shell_call_count(events: &[AgentEvent]) -> usize {
    use crate::event::EventType;
    let mut keys = std::collections::HashSet::new();
    for e in events.iter().filter(|e| matches!(e.event_type, EventType::ToolStarted | EventType::ToolFinished)) {
        if SHELL_TOOLS.contains(&event_tool(e).as_str()) {
            keys.insert(call_key(e));
        }
    }
    keys.len()
}

/// (tool calls seen, tool calls with both a start and a finish record). Calls are matched by
/// their tool-call id; events without an id count as unmatched calls of their own.
pub fn tool_call_pairs(events: &[AgentEvent]) -> (usize, usize) {
    use crate::event::EventType;
    let mut seen: HashMap<String, (bool, bool)> = HashMap::new();
    for e in events {
        let slot = match e.event_type {
            EventType::ToolStarted => 0,
            EventType::ToolFinished => 1,
            _ => continue,
        };
        let entry = seen.entry(call_key(e)).or_default();
        if slot == 0 { entry.0 = true } else { entry.1 = true }
    }
    (seen.len(), seen.values().filter(|(a, b)| *a && *b).count())
}

/// Results saved by older versions could say "All 0 ... passed", or pass "edits inside the folder"
/// while shell commands ran unseen. Re-reads them with today's rules (status and wording only;
/// nothing else is re-checked) and recomputes the overall verdict.
pub fn reread_legacy(task: &TaskDefinition, run: &mut crate::store::VerificationRun, events: &[AgentEvent]) {
    let mut changed = false;
    let shells = shell_call_count(events);
    for r in run.results.iter_mut().filter(|r| r.status == CheckStatus::Passed) {
        let note = "Re-read with TrailFox's newer, stricter wording; the files weren't checked again.".to_string();
        if r.check_type == "edits_inside_folder" && r.check_version == "1.0.0" {
            let n: usize = r.summary.strip_prefix("All ").and_then(|s| s.split_whitespace().next()).and_then(|x| x.parse().ok()).unwrap_or(1);
            if let Some((st, why)) = auto_checks::edits_coverage(n, shells) {
                r.status = st;
                r.summary = why;
                r.limitations.push(note);
                changed = true;
            }
        } else if r.summary.starts_with("All 0 ") {
            r.status = CheckStatus::NotApplicable;
            r.summary = format!("Nothing to check here. ({})", r.summary.trim_end_matches('.'));
            r.limitations.push(note);
            changed = true;
        }
    }
    if changed {
        let req: HashMap<&str, bool> = task.checks.iter().map(|c| (c.id.as_str(), c.is_required)).collect();
        run.overall_verdict = Some(overall_verdict(&run.results.iter().map(|r| (*req.get(r.check_id.as_str()).unwrap_or(&true), r.status)).collect::<Vec<_>>()));
    }
}

/// Registry of available verifiers keyed by check_type.
pub struct VerifierRegistry {
    verifiers: HashMap<&'static str, Box<dyn Verifier>>,
}

impl Default for VerifierRegistry {
    fn default() -> Self {
        let mut r = Self { verifiers: HashMap::new() };
        r.register(Box::new(originals_unchanged::OriginalsUnchanged));
        r.register(Box::new(required_outputs_exist::RequiredOutputsExist));
        r.register(Box::new(inputs_preserved_in_output::InputsPreservedInOutput));
        r.register(Box::new(folder_structure_matches::FolderStructureMatches));
        r.register(Box::new(changes_within_scope::ChangesWithinScope));
        r.register(Box::new(auto_checks::NoUnexpectedDeletions));
        r.register(Box::new(auto_checks::EditsInsideFolder));
        r
    }
}

impl VerifierRegistry {
    pub fn register(&mut self, v: Box<dyn Verifier>) {
        self.verifiers.insert(v.check_type(), v);
    }

    pub fn get(&self, check_type: &str) -> Option<&dyn Verifier> {
        self.verifiers.get(check_type).map(|b| b.as_ref())
    }

    pub fn types(&self) -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<_> = self.verifiers.values().map(|v| (v.check_type(), v.version())).collect();
        v.sort();
        v
    }

    /// Runs one configured check; unknown check types yield UnableToVerify.
    pub fn run(&self, check_id: &str, check_type: &str, params: &Value, evidence: &Evidence) -> CheckResult {
        let mut r = match self.get(check_type) {
            Some(v) => v.execute(params, evidence),
            None => {
                let now = crate::now_ms();
                CheckResult {
                    check_id: String::new(),
                    check_type: check_type.to_string(),
                    check_version: "unknown".into(),
                    status: CheckStatus::UnableToVerify,
                    summary: format!("This version of TrailFox doesn't know the check \"{check_type}\"."),
                    evidence_refs: vec![],
                    limitations: vec![],
                    started_at: now,
                    finished_at: now,
                }
            }
        };
        r.check_id = check_id.to_string();
        r
    }
}

/// Overall verdict (spec §8): any required Failed → Failed; any required UnableToVerify or
/// InsufficientCoverage → UnableToVerify; NotApplicable checks are left out; nothing left →
/// UnableToVerify; all remaining required Passed → Passed. Optional checks never change it.
pub fn overall_verdict(results: &[(bool, CheckStatus)]) -> CheckStatus {
    let required: Vec<CheckStatus> = results
        .iter()
        .filter(|(req, s)| *req && *s != CheckStatus::NotApplicable)
        .map(|(_, s)| if *s == CheckStatus::InsufficientCoverage { CheckStatus::UnableToVerify } else { *s })
        .collect();
    if required.is_empty() {
        CheckStatus::UnableToVerify
    } else if required.contains(&CheckStatus::Failed) {
        CheckStatus::Failed
    } else if required.contains(&CheckStatus::UnableToVerify) {
        CheckStatus::UnableToVerify
    } else {
        CheckStatus::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::CheckStatus::*;
    use super::*;

    #[test]
    fn verdict_rules() {
        assert_eq!(overall_verdict(&[]), UnableToVerify);
        assert_eq!(overall_verdict(&[(false, Passed)]), UnableToVerify);
        assert_eq!(overall_verdict(&[(true, Passed), (true, UnableToVerify), (true, Failed)]), Failed);
        assert_eq!(overall_verdict(&[(true, Passed), (true, UnableToVerify)]), UnableToVerify);
        assert_eq!(overall_verdict(&[(true, Passed), (false, Failed)]), Passed);
        assert_eq!(overall_verdict(&[(true, Passed), (true, NotApplicable)]), Passed);
        assert_eq!(overall_verdict(&[(true, NotApplicable)]), UnableToVerify);
        assert_eq!(overall_verdict(&[(true, Passed), (true, InsufficientCoverage)]), UnableToVerify);
    }

    #[test]
    fn check_kinds() {
        assert_eq!(check_kind("originals_unchanged"), CheckKind::Safety);
        assert_eq!(check_kind("edits_inside_folder"), CheckKind::Safety);
        assert_eq!(check_kind("required_outputs_exist"), CheckKind::OutputExists);
        assert_eq!(check_kind("inputs_preserved_in_output"), CheckKind::OutputContent);
    }

    fn ev(t: crate::event::EventType, tool: &str, id: &str) -> AgentEvent {
        AgentEvent {
            event_id: format!("{id}-{t:?}"),
            schema_version: "1".into(),
            connector_name: "test".into(),
            connector_version: "1".into(),
            agent_version: None,
            session_id: None,
            task_id: None,
            tool_call_id: Some(id.into()),
            parent_id: None,
            source_ts: None,
            collector_ts: 0,
            source_seq: None,
            working_dir: None,
            event_type: t,
            payload_redacted: serde_json::json!({"tool": tool}),
            capture_source: crate::event::CaptureSource::Hook,
            raw_source: None,
            collector_generated: false,
        }
    }

    #[test]
    fn shell_calls_and_pairs() {
        use crate::event::EventType::*;
        let evs = vec![ev(ToolStarted, "Bash", "a"), ev(ToolFinished, "Bash", "a"), ev(ToolStarted, "Write", "b"), ev(ToolFinished, "Write", "b"), ev(ToolStarted, "PowerShell", "c")];
        assert_eq!(shell_call_count(&evs), 2);
        let (calls, matched) = tool_call_pairs(&evs);
        assert_eq!((calls, matched), (3, 2));
    }
}
