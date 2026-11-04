//! "Changes within scope": every difference between the "before" and "after" snapshots lies inside
//! an allowed-write location. If the assertion covers areas that weren't snapshotted (or files
//! couldn't be read outside the allowed area), the result is unable_to_verify.
//! params: { "allowed": [..] (default: task allowed_write folders),
//!           "scope": [..] (folders the assertion covers; default: every snapshotted folder) }
use super::{abs_of, locate, str_list, CheckResult, Evidence, Findings, Verifier};
use crate::fs_observer::{relative_to, EntryType};
use crate::task::FolderRole;
use serde_json::Value;
use std::collections::BTreeSet;

pub struct ChangesWithinScope;

impl Verifier for ChangesWithinScope {
    fn check_type(&self) -> &'static str {
        "changes_within_scope"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["baseline_manifest", "verification_manifest", "change_hints?"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let mut allowed = str_list(params, "allowed");
        if allowed.is_empty() {
            allowed = ev.task_folders(FolderRole::AllowedWrite);
        }
        let allowed: Vec<String> = allowed.iter().map(|a| ev.resolve(a)).collect();
        let in_allowed = |p: &str| allowed.iter().any(|a| relative_to(a, p).is_some());

        if ev.baselines.is_empty() {
            f.uncertain.push("There is no \"before\" snapshot to compare against".into());
            return f.finish(self, String::new());
        }
        let mut scope: Vec<String> = str_list(params, "scope").iter().map(|s| ev.resolve(s)).collect();
        if scope.is_empty() {
            scope = ev.baselines.iter().map(|m| m.root.clone()).collect();
        }
        // Nested roots (e.g. a protected folder inside the working folder) would report the
        // same change twice; keep only the outermost ones.
        scope.sort();
        scope.dedup();
        let outer: Vec<String> = scope.iter().filter(|s| !scope.iter().any(|t| t != *s && relative_to(t, s).is_some())).cloned().collect();
        let scope = outer;
        let mut changed = 0usize;
        let mut covered_roots = Vec::new();
        for s in &scope {
            let (Some((bm, bprefix)), Some((vm, _))) = (locate(&ev.baselines, s), locate(&ev.verifications, s)) else {
                f.uncertain.push(format!("{s} wasn't snapshotted both before and after, so changes there can't be assessed"));
                continue;
            };
            covered_roots.push(s.clone());
            f.evidence.push(format!("baseline:{}", bm.id));
            f.evidence.push(format!("verification:{}", vm.id));
            let mut paths: BTreeSet<String> = BTreeSet::new();
            for e in &bm.entries {
                paths.insert(abs_of(bm, &e.rel_path));
            }
            for e in &vm.entries {
                paths.insert(abs_of(vm, &e.rel_path));
            }
            for p in paths {
                if relative_to(s, &p).is_none() || (bprefix.is_empty() && p == bm.root) {
                    continue;
                }
                let b = relative_to(&bm.root, &p).and_then(|r| bm.get(&r).cloned());
                let v = relative_to(&vm.root, &p).and_then(|r| vm.get(&r).cloned());
                let kind = match (&b, &v) {
                    (None, Some(_)) => Some("added"),
                    (Some(_), None) => Some("deleted"),
                    (Some(b), Some(v)) => {
                        if b.entry_type != v.entry_type {
                            Some("changed type")
                        } else if b.entry_type == EntryType::File && (!b.trustworthy() || !v.trustworthy()) {
                            if !in_allowed(&p) {
                                f.uncertain.push(format!("Couldn't read {p} to tell whether it changed"));
                            }
                            None
                        } else if b.entry_type == EntryType::File && b.sha256 != v.sha256 {
                            Some("modified")
                        } else {
                            None
                        }
                    }
                    (None, None) => None,
                };
                if let Some(kind) = kind {
                    changed += 1;
                    if !in_allowed(&p) {
                        f.failures.push(format!("{p} was {kind} outside the allowed area"));
                    }
                }
            }
        }
        // Live change hints outside both the allowed area and the snapshots: we saw activity we
        // can't assess with hashes.
        let mut transient = 0usize;
        for h in &ev.change_hints {
            let covered = covered_roots.iter().any(|r| relative_to(r, &h.path).is_some());
            if covered && !in_allowed(&h.path) {
                transient += 1;
            }
        }
        if transient > 0 {
            f.limitations.push(format!(
                "{transient} live file event(s) were seen outside the allowed area; if the files ended up identical to the \"before\" snapshot they are not reported as changes"
            ));
        }
        if allowed.is_empty() {
            f.limitations.push("No allowed-write folders are configured, so any change counts as out of scope.".into());
        }
        f.limitations.push("Only folders that were snapshotted are covered; changes elsewhere on the PC are not visible to this check.".into());
        f.finish(self, format!("{changed} change(s) found, all inside the allowed area."))
    }
}
