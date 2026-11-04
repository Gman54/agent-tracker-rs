//! "Originals unchanged": every baseline entry under the protected paths is still present with the
//! same type and SHA-256 in the verification manifest. New files under protected paths also fail
//! unless `allow_additions` is true.
//!
//! params: { "paths": [..] (default: task protected folders), "allow_additions": false }
use super::{abs_of, entries_under, locate, str_list, CheckResult, Evidence, Findings, Verifier};
use crate::fs_observer::EntryType;
use crate::task::FolderRole;
use serde_json::Value;

pub struct OriginalsUnchanged;

impl Verifier for OriginalsUnchanged {
    fn check_type(&self) -> &'static str {
        "originals_unchanged"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["baseline_manifest", "verification_manifest"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let mut paths = str_list(params, "paths");
        if paths.is_empty() {
            paths = ev.task_folders(FolderRole::Protected);
        }
        let allow_additions = params.get("allow_additions").and_then(Value::as_bool).unwrap_or(false);
        if paths.is_empty() {
            f.uncertain.push("No protected files or folders are configured".into());
            return f.finish(self, String::new());
        }
        let mut compared = 0usize;
        for p in &paths {
            let abs = ev.resolve(p);
            let Some((bm, prefix)) = locate(&ev.baselines, &abs) else {
                f.uncertain.push(format!("No \"before\" snapshot covers {abs}"));
                continue;
            };
            let Some((vm, vprefix)) = locate(&ev.verifications, &abs) else {
                f.uncertain.push(format!("No \"after\" snapshot covers {abs}"));
                continue;
            };
            f.evidence.push(format!("baseline:{}", bm.id));
            f.evidence.push(format!("verification:{}", vm.id));
            // Baseline/verification may have different roots; compare by absolute path.
            let base_entries: Vec<_> = entries_under(bm, &prefix).collect();
            if base_entries.is_empty() && !prefix.is_empty() {
                f.uncertain.push(format!("{abs} did not exist in the \"before\" snapshot"));
                continue;
            }
            for be in &base_entries {
                let full = abs_of(bm, &be.rel_path);
                let vrel = crate::fs_observer::relative_to(&vm.root, &full).unwrap_or_default();
                compared += 1;
                match vm.get(&vrel) {
                    None => f.failures.push(format!("{} was deleted or moved", be.rel_path)),
                    Some(ve) if ve.entry_type != be.entry_type => f.failures.push(format!("{} changed type", be.rel_path)),
                    Some(ve) if be.entry_type == EntryType::File => {
                        if !be.trustworthy() || !ve.trustworthy() {
                            let why = ve.error_detail.clone().or_else(|| be.error_detail.clone()).unwrap_or_else(|| "unreadable".into());
                            f.uncertain.push(format!("Couldn't confirm {} ({why})", be.rel_path));
                        } else if be.sha256 != ve.sha256 {
                            f.failures.push(format!("{} was modified", be.rel_path));
                        }
                    }
                    Some(_) => {}
                }
            }
            for ve in entries_under(vm, &vprefix) {
                let full = abs_of(vm, &ve.rel_path);
                let brel = crate::fs_observer::relative_to(&bm.root, &full).unwrap_or_default();
                if bm.get(&brel).is_none() && !allow_additions {
                    f.failures.push(format!("{} was added inside a protected area", ve.rel_path));
                }
            }
        }
        f.limitations.push("Compares file contents by SHA-256 at two moments; a change that was made and then reverted in between is not detectable here.".into());
        f.finish(self, format!("All {compared} protected item(s) are unchanged."))
    }
}
