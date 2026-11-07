//! Checks used by tasks that start automatically when an agent receives a prompt.
//!
//! - "no_unexpected_deletions": every file in the "before" snapshot is still there afterwards,
//!   unless it is listed in `allowed_deletions`. params: { "allowed_deletions": [..] }
//! - "edits_inside_folder": every file the agent reported creating or editing lies inside the
//!   task folder. Based on the agent's own reports; shell commands can't be judged.
//!   params: { "folder": ".." (default: the task's first working folder) }
use super::{abs_of, str_list, CheckResult, Evidence, Findings, Verifier};
use crate::event::{AgentEvent, EventType};
use crate::fs_observer::{normalize_path, relative_to, EntryType};
use crate::task::FolderRole;
use serde_json::Value;
use std::path::Path;

/// Tools whose input names a file the agent creates or changes.
const EDIT_TOOLS: &[&str] = &["write", "edit", "multiedit", "notebookedit", "create", "create_file", "edit_file", "str_replace", "write_file", "apply_patch"];

/// Files the agent said it created or edited (absolute, normalized, de-duplicated, in order).
/// Failed tool calls are ignored.
pub fn reported_edit_paths(events: &[AgentEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in events.iter().filter(|e| e.event_type == EventType::ToolFinished) {
        let p = &e.payload_redacted;
        if p.get("failed").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let mut found: Vec<String> = Vec::new();
        // Cursor: {"hook": "afterFileEdit", "data": {"file_path": ..}}
        if p.get("hook").and_then(Value::as_str) == Some("afterFileEdit") {
            if let Some(f) = p.pointer("/data/file_path").and_then(Value::as_str) {
                found.push(f.to_string());
            }
        }
        // Claude Code / Copilot: {"tool": "Write", "input": {"file_path": ..}}
        let tool = p.get("tool").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        if EDIT_TOOLS.contains(&tool.as_str()) {
            let input = match p.get("input") {
                Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
                Some(v) => v.clone(),
                None => Value::Null,
            };
            for k in ["file_path", "notebook_path", "path", "filePath"] {
                if let Some(f) = input.get(k).and_then(Value::as_str) {
                    found.push(f.to_string());
                    break;
                }
            }
        }
        for f in found {
            if f.contains("[REDACTED") || f.trim().is_empty() {
                continue;
            }
            let path = Path::new(&f);
            let abs = if path.is_absolute() || f.starts_with('/') || f.starts_with('\\') {
                normalize_path(path)
            } else if let Some(wd) = &e.working_dir {
                normalize_path(&Path::new(wd).join(path))
            } else {
                continue;
            };
            if !out.contains(&abs) {
                out.push(abs);
            }
        }
    }
    out
}

pub struct NoUnexpectedDeletions;

impl Verifier for NoUnexpectedDeletions {
    fn check_type(&self) -> &'static str {
        "no_unexpected_deletions"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["baseline_manifest", "verification_manifest"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let allowed: Vec<String> = str_list(params, "allowed_deletions").iter().map(|a| ev.resolve(a)).collect();
        if ev.baselines.is_empty() {
            f.uncertain.push("There is no \"before\" snapshot to compare against".into());
            return f.finish(self, String::new());
        }
        let mut files = 0usize;
        for bm in &ev.baselines {
            let Some(vm) = ev.verifications.iter().find(|v| v.root == bm.root) else {
                f.uncertain.push(format!("{} wasn't snapshotted afterwards", bm.root));
                continue;
            };
            if !vm.root_ok {
                f.uncertain.push(format!("{} couldn't be read afterwards", bm.root));
                continue;
            }
            f.evidence.push(format!("baseline:{}", bm.id));
            f.evidence.push(format!("verification:{}", vm.id));
            for e in bm.entries.iter().filter(|e| e.entry_type == EntryType::File && !e.rel_path.is_empty()) {
                files += 1;
                if vm.get(&e.rel_path).is_none() {
                    let abs = abs_of(bm, &e.rel_path);
                    if !allowed.iter().any(|a| relative_to(a, &abs).is_some()) {
                        f.failures.push(format!("{abs} was deleted"));
                    }
                }
            }
        }
        f.finish(self, format!("All {files} file(s) that were there before are still there."))
    }
}

pub struct EditsInsideFolder;

impl Verifier for EditsInsideFolder {
    fn check_type(&self) -> &'static str {
        "edits_inside_folder"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["agent_events"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let folder = match params.get("folder").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => ev.resolve(s),
            _ => match ev.task_folders(FolderRole::Working).into_iter().next() {
                Some(w) => normalize_path(Path::new(&w)),
                None => {
                    f.uncertain.push("The task has no project folder".into());
                    return f.finish(self, String::new());
                }
            },
        };
        if ev.events.is_empty() {
            f.uncertain.push("No messages arrived from the agent, so there is nothing to compare".into());
            return f.finish(self, String::new());
        }
        let paths = reported_edit_paths(&ev.events);
        for p in &paths {
            if relative_to(&folder, p).is_none() {
                f.failures.push(format!("The agent changed {p}, which is outside {folder}"));
            }
        }
        f.limitations.push("Based on the file edits the agent reported. Changes made through shell commands outside the folder can't be seen.".into());
        f.finish(self, format!("All {} file edit(s) the agent reported are inside the project folder.", paths.len()))
    }
}
