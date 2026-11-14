//! Checks used by tasks that start automatically when an agent receives a prompt.
//!
//! - "no_unexpected_deletions": every file in the "before" snapshot, and in the quick name list
//!   taken when the request arrived, is still there afterwards, unless it is listed in
//!   `allowed_deletions`. params: { "allowed_deletions": [..] }
//! - "edits_inside_folder": every file the agent reported creating or editing lies inside the
//!   task folder. Based on the agent's own reports; shell commands can't be judged.
//!   params: { "folder": ".." (default: the task's first working folder) }
use super::{abs_of, shell_call_count, str_list, CheckResult, CheckStatus, Evidence, Findings, Verifier};
use crate::event::{AgentEvent, EventType};
use crate::fs_observer::{normalize_path, relative_to, EntryType};
use crate::quick_list::thousands;
use crate::task::FolderRole;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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
        "1.2.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["baseline_manifest", "verification_manifest"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let allowed: Vec<String> = str_list(params, "allowed_deletions").iter().map(|a| ev.resolve(a)).collect();
        let is_allowed = |abs: &str| allowed.iter().any(|a| relative_to(a, abs).is_some());
        if ev.baselines.is_empty() && ev.quick_lists.is_empty() {
            f.uncertain.push("There is no \"before\" snapshot to compare against".into());
            return f.finish(self, String::new());
        }
        // What is there now, per folder (only folders that could be read afterwards).
        let after: HashMap<&str, HashSet<&str>> =
            ev.verifications.iter().filter(|v| v.root_ok).map(|v| (v.root.as_str(), v.entries.iter().map(|e| e.rel_path.as_str()).collect())).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let (mut base_files, mut base_missing) = (0usize, Vec::<String>::new());
        for bm in &ev.baselines {
            let Some(vm) = ev.verifications.iter().find(|v| v.root == bm.root) else {
                f.uncertain.push(format!("{} wasn't snapshotted afterwards", bm.root));
                continue;
            };
            if !vm.root_ok {
                f.uncertain.push(format!("{} couldn't be read afterwards", bm.root));
                continue;
            }
            let now = &after[vm.root.as_str()];
            f.evidence.push(format!("baseline:{}", bm.id));
            f.evidence.push(format!("verification:{}", vm.id));
            for e in bm.entries.iter().filter(|e| e.entry_type == EntryType::File && !e.rel_path.is_empty()) {
                base_files += 1;
                let abs = abs_of(bm, &e.rel_path);
                seen.insert(abs.clone());
                if !now.contains(e.rel_path.as_str()) && !is_allowed(&abs) {
                    base_missing.push(abs);
                }
            }
        }
        // Files listed the moment the request arrived. Anything here that the full snapshot
        // never saw and that is gone now was deleted before the full snapshot reached it.
        let (mut quick_files, mut quick_missing, mut problems) = (0usize, Vec::<String>::new(), Vec::<String>::new());
        for ql in &ev.quick_lists {
            if let Some(p) = ql.problem() {
                problems.push(p);
            }
            let Some(now) = after.get(ql.root.as_str()) else {
                let msg = format!("{} wasn't snapshotted afterwards", ql.root);
                if !f.uncertain.contains(&msg) && !f.uncertain.iter().any(|u| u.starts_with(&ql.root)) {
                    f.uncertain.push(msg);
                }
                continue;
            };
            f.evidence.push(format!("quick_name_list:{}", ql.id));
            for e in &ql.entries {
                let abs = format!("{}/{}", ql.root.trim_end_matches('/'), e.rel_path);
                if !seen.insert(abs.clone()) {
                    continue;
                }
                quick_files += 1;
                if !now.contains(e.rel_path.as_str()) && !is_allowed(&abs) {
                    quick_missing.push(abs);
                }
            }
        }
        if !quick_missing.is_empty() {
            f.failures.push(format!(
                "Deleted soon after you sent the request, before the full \"before\" snapshot reached {} (seen in the quick name list): {}",
                if quick_missing.len() == 1 { "it" } else { "them" },
                name_list(&quick_missing)
            ));
        }
        if !base_missing.is_empty() {
            f.failures.push(format!("Deleted after the full \"before\" snapshot: {}", name_list(&base_missing)));
        }
        let quick_used = !ev.quick_lists.is_empty();
        let early = matches!((ev.baseline_finished_at, crate::quick_list::first_action_at(&ev.events)), (Some(done), Some(a)) if a < done);
        for p in &problems {
            if early {
                // The race this list guards against may really have happened: don't pass.
                f.uncertain.push(format!("{p}, and the agent started working before the full snapshot finished, so a file deleted early might have been missed"));
            } else {
                f.limitations.push(format!("{p}. The full \"before\" snapshot was still checked."));
            }
        }
        if !quick_used && ev.task.as_ref().map(|t| t.is_auto()).unwrap_or(false) {
            f.limitations.push("No quick name list was taken when the request arrived, so only the full \"before\" snapshot was used.".into());
        }
        if early && quick_used && problems.is_empty() {
            f.limitations.push(crate::quick_list::EARLY_START_NOTE.into());
        }
        let total = base_files + quick_files;
        if total == 0 && f.failures.is_empty() && f.uncertain.is_empty() {
            f.neutral = Some((CheckStatus::NotApplicable, "There were no files in the \"before\" snapshot, so there was nothing to lose.".into()));
        }
        let pass = if quick_used && problems.is_empty() {
            format!(
                "All {} files that were there when you sent the request are still there. Checked against both the quick name list taken when you sent the request and the full \"before\" snapshot ({} files).",
                thousands(total),
                thousands(base_files)
            )
        } else if quick_used {
            format!(
                "All {} files that were seen before are still there, but this may not be the whole folder. {}. The full \"before\" snapshot ({} files) was still checked.",
                thousands(total),
                problems.join(". "),
                thousands(base_files)
            )
        } else {
            format!("All {} file(s) that were there before are still there.", thousands(base_files))
        };
        f.finish(self, pass)
    }
}

/// How many deleted files are named before "and N more".
pub const NAMED_DELETIONS: usize = 20;

/// "a, b, c, and 4 more"
fn name_list(paths: &[String]) -> String {
    let mut s = paths.iter().take(NAMED_DELETIONS).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > NAMED_DELETIONS {
        s.push_str(&format!(", and {} more", thousands(paths.len() - NAMED_DELETIONS)));
    }
    s
}

pub struct EditsInsideFolder;

impl Verifier for EditsInsideFolder {
    fn check_type(&self) -> &'static str {
        "edits_inside_folder"
    }
    fn version(&self) -> &'static str {
        "1.1.0"
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
        if f.failures.is_empty() {
            f.neutral = edits_coverage(paths.len(), shell_call_count(&ev.events));
        }
        f.finish(self, format!("All {} file edit(s) the agent reported are inside the project folder.", paths.len()))
    }
}

/// How "edits inside the folder" should read when nothing was outside: not applicable when the
/// agent reported no edits and ran no shell commands, insufficient coverage when shell commands
/// ran (their changes are invisible here), otherwise None (a real pass).
pub fn edits_coverage(n: usize, shells: usize) -> Option<(CheckStatus, String)> {
    if shells > 0 {
        let mut s = format!("Insufficient coverage: the agent ran {shells} shell command(s). Changes made by shell commands aren't visible to this check.");
        if n > 0 {
            s.push_str(&format!(" The {n} file edit(s) it reported are inside the project folder."));
        }
        Some((CheckStatus::InsufficientCoverage, s))
    } else if n == 0 {
        Some((CheckStatus::NotApplicable, "No applicable reported edits: the agent didn't report any file edits.".into()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_observer::{Manifest, ManifestEntry, ManifestType, Stability};
    use crate::quick_list::{QuickEntry, QuickList};
    use serde_json::json;

    const ROOT: &str = "/home/greg/downloads";

    fn manifest(t: ManifestType, files: &[&str]) -> Manifest {
        Manifest {
            id: format!("{}-m", t.as_str()),
            task_id: Some("t1".into()),
            root: ROOT.into(),
            captured_at: 0,
            manifest_type: t,
            entries: files
                .iter()
                .map(|f| ManifestEntry {
                    rel_path: f.to_string(),
                    entry_type: EntryType::File,
                    size_bytes: Some(1),
                    sha256: Some("x".into()),
                    modified_ts: Some(0),
                    observed_ts: 0,
                    read_ok: true,
                    error_detail: None,
                    stability: Stability::Stable,
                })
                .collect(),
            root_ok: true,
        }
    }

    fn quick(files: &[&str]) -> QuickList {
        QuickList {
            id: "q1".into(),
            root: ROOT.into(),
            received_at: 1_000,
            started_at: Some(1_001),
            finished_at: Some(1_050),
            file_count: files.len(),
            entries: files.iter().map(|f| QuickEntry { rel_path: f.to_string(), size: 1, mtime: Some(0) }).collect(),
            ..Default::default()
        }
    }

    fn tool_event(ts: i64) -> AgentEvent {
        let mut e = AgentEvent::new("claude_code", "1", EventType::ToolStarted, crate::event::CaptureSource::Hook, None);
        e.source_ts = Some(ts);
        e
    }

    fn run(params: Value, ev: &Evidence) -> CheckResult {
        NoUnexpectedDeletions.execute(&params, ev)
    }

    /// The reported case: the agent deleted testmajor120.json before the full snapshot reached it.
    #[test]
    fn a_file_deleted_before_the_full_snapshot_is_caught_by_the_quick_list() {
        let ev = Evidence {
            baselines: vec![manifest(ManifestType::Baseline, &["a.txt", "b.txt"])],
            verifications: vec![manifest(ManifestType::Verification, &["a.txt", "b.txt"])],
            quick_lists: vec![quick(&["a.txt", "b.txt", "testmajor120.json"])],
            events: vec![tool_event(2_000)],
            baseline_finished_at: Some(90_000),
            ..Default::default()
        };
        let r = run(json!({}), &ev);
        assert_eq!(r.status, CheckStatus::Failed, "{}", r.summary);
        assert!(r.summary.contains("/home/greg/downloads/testmajor120.json"), "{}", r.summary);
        assert!(r.summary.contains("quick name list"), "{}", r.summary);
        assert!(r.evidence_refs.iter().any(|e| e == "quick_name_list:q1"));
        // Allowed deletions are respected for the quick list too.
        let r = run(json!({"allowed_deletions": ["/home/greg/downloads/testmajor120.json"]}), &ev);
        assert_eq!(r.status, CheckStatus::Passed, "{}", r.summary);
    }

    #[test]
    fn many_deletions_are_named_then_counted() {
        let names: Vec<String> = (0..25).map(|i| format!("f{i:02}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ev = Evidence {
            baselines: vec![manifest(ManifestType::Baseline, &refs[..3])],
            verifications: vec![manifest(ManifestType::Verification, &[])],
            quick_lists: vec![quick(&refs)],
            ..Default::default()
        };
        let r = run(json!({}), &ev);
        assert_eq!(r.status, CheckStatus::Failed);
        assert!(r.summary.contains("Deleted after the full \"before\" snapshot: /home/greg/downloads/f00.txt"), "{}", r.summary);
        assert!(r.summary.contains("/home/greg/downloads/f22.txt, and 2 more"), "22 quick-only files: 20 named, 2 counted: {}", r.summary);
    }

    #[test]
    fn a_cut_short_quick_list_is_said_plainly() {
        let mut q = quick(&["a.txt"]);
        q.cut_short = true;
        q.cut_reason = Some("it stops at 1 files".into());
        // Agent started after the full snapshot finished: still a pass, but it says so.
        let mut ev = Evidence {
            baselines: vec![manifest(ManifestType::Baseline, &["a.txt", "b.txt"])],
            verifications: vec![manifest(ManifestType::Verification, &["a.txt", "b.txt"])],
            quick_lists: vec![q],
            events: vec![tool_event(95_000)],
            baseline_finished_at: Some(90_000),
            ..Default::default()
        };
        let r = run(json!({}), &ev);
        assert_eq!(r.status, CheckStatus::Passed);
        assert!(r.summary.contains("was cut short after 1 files"), "{}", r.summary);
        assert!(r.summary.contains("may not be the whole folder"), "{}", r.summary);
        // Agent started before the full snapshot finished: it can't vouch for that, so no pass.
        ev.events = vec![tool_event(2_000)];
        let r = run(json!({}), &ev);
        assert_eq!(r.status, CheckStatus::UnableToVerify, "{}", r.summary);
        assert!(r.summary.contains("cut short") && r.summary.contains("before the full snapshot finished"), "{}", r.summary);
        // A list that failed is reported too.
        let mut q = quick(&[]);
        q.error = Some("couldn't open the folder: denied".into());
        ev.quick_lists = vec![q];
        let r = run(json!({}), &ev);
        assert_eq!(r.status, CheckStatus::UnableToVerify);
        assert!(r.summary.contains("didn't work"), "{}", r.summary);
    }

    #[test]
    fn normal_pass_mentions_both_lists() {
        let files: Vec<String> = (0..1500).map(|i| format!("f{i}.txt")).collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let ev = Evidence {
            baselines: vec![manifest(ManifestType::Baseline, &refs)],
            verifications: vec![manifest(ManifestType::Verification, &refs)],
            quick_lists: vec![quick(&refs)],
            events: vec![tool_event(2_000)],
            baseline_finished_at: Some(1_900),
            ..Default::default()
        };
        let r = run(json!({}), &ev);
        assert_eq!(r.status, CheckStatus::Passed);
        assert_eq!(
            r.summary,
            "All 1,500 files that were there when you sent the request are still there. Checked against both the quick name list taken when you sent the request and the full \"before\" snapshot (1,500 files)."
        );
        assert!(r.limitations.is_empty(), "{:?}", r.limitations);
        // Without a quick list (a manual task) the old wording stays.
        let ev = Evidence { quick_lists: vec![], ..ev };
        assert_eq!(run(json!({}), &ev).summary, "All 1,500 file(s) that were there before are still there.");
    }

    #[test]
    fn edits_check_never_says_all_zero() {
        let (st, why) = edits_coverage(0, 0).unwrap();
        assert_eq!(st, CheckStatus::NotApplicable);
        assert!(why.starts_with("No applicable reported edits"));
        let (st, why) = edits_coverage(0, 4).unwrap();
        assert_eq!(st, CheckStatus::InsufficientCoverage);
        assert!(why.contains("4 shell command(s)") && !why.contains("All 0"));
        assert!(edits_coverage(2, 1).unwrap().1.contains("The 2 file edit(s) it reported are inside"));
        assert_eq!(edits_coverage(3, 0), None, "a real pass");
    }
}
