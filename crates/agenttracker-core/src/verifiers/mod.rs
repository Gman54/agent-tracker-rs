//! Verifier plugins. Each check inspects evidence (manifests, events, change hints) and returns
//! Passed / Failed / UnableToVerify. A check must say "unable to verify" whenever the evidence
//! doesn't cover what it asserts — never guess "passed".
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
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::UnableToVerify => "unable_to_verify",
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
    pub evidence: Vec<String>,
    pub limitations: Vec<String>,
}

impl Findings {
    pub fn new() -> Self {
        Self { started_at: crate::now_ms(), failures: vec![], uncertain: vec![], evidence: vec![], limitations: vec![] }
    }

    pub fn finish(self, v: &dyn Verifier, pass_summary: String) -> CheckResult {
        let (status, summary) = if !self.failures.is_empty() {
            (CheckStatus::Failed, summarize(&self.failures))
        } else if !self.uncertain.is_empty() {
            (CheckStatus::UnableToVerify, summarize(&self.uncertain))
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
                    summary: format!("This version of AgentTracker doesn't know the check \"{check_type}\"."),
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

/// Overall verdict (spec §8): any required Failed → Failed; any required UnableToVerify →
/// UnableToVerify; no required checks → UnableToVerify; all required Passed → Passed.
/// Optional checks never change the verdict.
pub fn overall_verdict(results: &[(bool, CheckStatus)]) -> CheckStatus {
    let required: Vec<CheckStatus> = results.iter().filter(|(req, _)| *req).map(|(_, s)| *s).collect();
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
    }
}
