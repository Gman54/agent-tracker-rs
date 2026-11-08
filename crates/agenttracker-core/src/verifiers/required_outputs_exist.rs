//! "Required outputs exist": each configured path exists in the verification snapshot and was
//! readable. params: { "paths": [..], "min_size_bytes": 0 }
use super::{locate, str_list, CheckResult, Evidence, Findings, Verifier};
use crate::fs_observer::{EntryType, Stability};
use serde_json::Value;

pub struct RequiredOutputsExist;

impl Verifier for RequiredOutputsExist {
    fn check_type(&self) -> &'static str {
        "required_outputs_exist"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["verification_manifest"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let paths = str_list(params, "paths");
        let min_size = params.get("min_size_bytes").and_then(Value::as_u64).unwrap_or(0);
        if paths.is_empty() {
            f.uncertain.push("No required outputs are configured".into());
            return f.finish(self, String::new());
        }
        for p in &paths {
            let abs = ev.resolve(p);
            let Some((vm, rel)) = locate(&ev.verifications, &abs) else {
                f.uncertain.push(format!("{abs} is outside every folder TrailFox snapshotted"));
                continue;
            };
            f.evidence.push(format!("verification:{}:{rel}", vm.id));
            match vm.get(&rel) {
                None => f.failures.push(format!("{p} is missing")),
                Some(e) if e.entry_type == EntryType::Dir => {}
                Some(e) if e.entry_type != EntryType::File => f.failures.push(format!("{p} is not a regular file")),
                Some(e) if !e.read_ok => {
                    f.uncertain.push(format!("{p} exists but couldn't be read ({})", e.error_detail.clone().unwrap_or_default()))
                }
                Some(e) => {
                    if e.stability == Stability::Unstable {
                        f.limitations.push(format!("{p} was still changing while it was checked"));
                    }
                    if e.size_bytes.unwrap_or(0) < min_size {
                        f.failures.push(format!("{p} is smaller than {min_size} bytes"));
                    }
                }
            }
        }
        f.finish(self, format!("All {} required output(s) exist and are readable.", paths.len()))
    }
}
