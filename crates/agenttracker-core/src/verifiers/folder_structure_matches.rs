//! "Folder structure matches": files under `root` follow approved path/naming patterns.
//! params: { "root": path, "patterns": ["*.txt", "reports/**/*.md"], "required_patterns": [..],
//!           "allow_other": false }
//! Patterns are globs relative to root; `*` does not cross folders, `**` does. Case-insensitive.
use super::{entries_under, locate, str_list, CheckResult, Evidence, Findings, Verifier};
use crate::fs_observer::EntryType;
use globset::{Glob, GlobBuilder, GlobMatcher};
use serde_json::Value;

pub struct FolderStructureMatches;

fn compile(p: &str) -> Result<GlobMatcher, globset::Error> {
    let _ = Glob::new(p)?;
    Ok(GlobBuilder::new(p).literal_separator(true).case_insensitive(true).build()?.compile_matcher())
}

impl Verifier for FolderStructureMatches {
    fn check_type(&self) -> &'static str {
        "folder_structure_matches"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn required_evidence(&self) -> &[&'static str] {
        &["verification_manifest"]
    }

    fn execute(&self, params: &Value, ev: &Evidence) -> CheckResult {
        let mut f = Findings::new();
        let patterns = str_list(params, "patterns");
        let required = str_list(params, "required_patterns");
        let allow_other = params.get("allow_other").and_then(Value::as_bool).unwrap_or(false);
        let Some(root) = params.get("root").and_then(Value::as_str) else {
            f.uncertain.push("No folder is configured for the structure check".into());
            return f.finish(self, String::new());
        };
        if patterns.is_empty() && required.is_empty() {
            f.uncertain.push("No naming patterns are configured".into());
            return f.finish(self, String::new());
        }
        let mut compiled = Vec::new();
        for p in patterns.iter().chain(required.iter()) {
            match compile(p) {
                Ok(m) => compiled.push((p.clone(), m)),
                Err(e) => {
                    f.uncertain.push(format!("Pattern \"{p}\" is not valid: {e}"));
                }
            }
        }
        if !f.uncertain.is_empty() {
            return f.finish(self, String::new());
        }
        let abs = ev.resolve(root);
        let Some((vm, prefix)) = locate(&ev.verifications, &abs) else {
            f.uncertain.push(format!("No \"after\" snapshot covers {abs}"));
            return f.finish(self, String::new());
        };
        f.evidence.push(format!("verification:{}", vm.id));
        let strip = |rel: &str| if prefix.is_empty() { rel.to_string() } else { rel[prefix.len() + 1..].to_string() };
        let files: Vec<String> = entries_under(vm, &prefix).filter(|e| e.entry_type == EntryType::File && e.rel_path != prefix).map(|e| strip(&e.rel_path)).collect();
        let allowed: Vec<&GlobMatcher> = compiled.iter().filter(|(p, _)| patterns.contains(p)).map(|(_, m)| m).collect();
        if !allow_other && !allowed.is_empty() {
            for file in &files {
                if !allowed.iter().any(|m| m.is_match(file)) {
                    f.failures.push(format!("{file} doesn't match the approved names"));
                }
            }
        }
        for rp in &required {
            let m = compile(rp).expect("validated above");
            if !files.iter().any(|file| m.is_match(file)) {
                f.failures.push(format!("Nothing matches the required pattern \"{rp}\""));
            }
        }
        f.finish(self, format!("{} file(s) follow the approved structure.", files.len()))
    }
}
