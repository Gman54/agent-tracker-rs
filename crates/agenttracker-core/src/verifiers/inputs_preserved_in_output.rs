//! "Inputs preserved in output": every input file's content appears in the output folder at least
//! as many times as it appears in the input (hash multiplicity — two identical inputs need two
//! matching outputs). params: { "input": path, "output": path }
use super::{entries_under, locate, CheckResult, Evidence, Findings, Verifier};
use crate::fs_observer::{EntryType, Manifest};
use serde_json::Value;
use std::collections::BTreeMap;

pub struct InputsPreservedInOutput;

impl Verifier for InputsPreservedInOutput {
    fn check_type(&self) -> &'static str {
        "inputs_preserved_in_output"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["baseline_manifest", "verification_manifest"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let (Some(input), Some(output)) = (params.get("input").and_then(Value::as_str), params.get("output").and_then(Value::as_str)) else {
            f.uncertain.push("Input and output folders must both be configured".into());
            return f.finish(self, String::new());
        };
        let (in_abs, out_abs) = (ev.resolve(input), ev.resolve(output));
        // Input contents come from the "before" snapshot (what existed when the task started).
        let in_src: Option<(&Manifest, String)> = match locate(&ev.baselines, &in_abs) {
            Some(x) => Some(x),
            None => locate(&ev.verifications, &in_abs).inspect(|_| {
                f.limitations.push("No \"before\" snapshot of the input folder; used the \"after\" snapshot instead".into());
            }),
        };
        let Some((im, iprefix)) = in_src else {
            f.uncertain.push(format!("No snapshot covers the input folder {in_abs}"));
            return f.finish(self, String::new());
        };
        let Some((om, oprefix)) = locate(&ev.verifications, &out_abs) else {
            f.uncertain.push(format!("No \"after\" snapshot covers the output folder {out_abs}"));
            return f.finish(self, String::new());
        };
        f.evidence.push(format!("input:{}", im.id));
        f.evidence.push(format!("output:{}", om.id));

        let mut need: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for e in entries_under(im, &iprefix).filter(|e| e.entry_type == EntryType::File) {
            match (&e.sha256, e.trustworthy()) {
                (Some(h), true) => need.entry(h.clone()).or_default().push(e.rel_path.clone()),
                _ => f.uncertain.push(format!("Couldn't read input {}", e.rel_path)),
            }
        }
        let mut have: BTreeMap<String, usize> = BTreeMap::new();
        let mut unreadable_outputs = 0usize;
        for e in entries_under(om, &oprefix).filter(|e| e.entry_type == EntryType::File) {
            match (&e.sha256, e.trustworthy()) {
                (Some(h), true) => *have.entry(h.clone()).or_default() += 1,
                _ => unreadable_outputs += 1,
            }
        }
        if need.is_empty() && f.uncertain.is_empty() {
            f.uncertain.push("The input folder has no files, so there is nothing to compare".into());
        }
        let total: usize = need.values().map(Vec::len).sum();
        let mut shortfalls = Vec::new();
        for (hash, inputs) in &need {
            let got = have.get(hash).copied().unwrap_or(0);
            if got < inputs.len() {
                shortfalls.push(format!("{} (needs {} matching output(s), found {got})", inputs.join(", "), inputs.len()));
            }
        }
        if !shortfalls.is_empty() {
            if unreadable_outputs > 0 {
                // The missing copies might be among the files we couldn't read.
                f.uncertain.extend(shortfalls.into_iter().map(|s| format!("{s}; {unreadable_outputs} output file(s) couldn't be read")));
            } else {
                f.failures.extend(shortfalls.into_iter().map(|s| format!("Missing from output: {s}")));
            }
        }
        f.limitations.push("Matches by exact file content (SHA-256); renamed copies count, edited copies don't.".into());
        f.finish(self, format!("All {total} input file(s) are present in the output with matching content."))
    }
}
