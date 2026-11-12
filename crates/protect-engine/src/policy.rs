//! Profiles, typed operation arguments and deterministic decisions. A model never edits these:
//! revisions are created only through the admin path.
use crate::errors::{Code, ProtectError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const DEFAULT_PROFILE: &str = "staged-project-v1";
/// Operations the engine knows. Anything else is refused, never mapped to a general operation.
pub const OPERATIONS: &[&str] = &["files.list", "files.read", "files.write_staged", "files.diff"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    RequireApproval,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::RequireApproval => "require_approval",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub profile: String,
    pub operations: BTreeMap<String, Decision>,
    /// Path names or simple `*.ext` patterns that are always refused (any folder level).
    pub deny_paths: Vec<String>,
    pub max_actions: u64,
    pub max_file_bytes: u64,
    pub max_session_minutes: u64,
    pub approval_minutes: u64,
    /// Staging and snapshot caps for the file worker. Defaulted so policies saved before the
    /// worker existed still load.
    #[serde(default = "d_staged_files")]
    pub max_staged_files: u64,
    #[serde(default = "d_staging_bytes")]
    pub max_staging_bytes: u64,
    #[serde(default = "d_snapshot_files")]
    pub max_snapshot_files: u64,
    #[serde(default = "d_snapshot_bytes")]
    pub max_snapshot_bytes: u64,
}

fn d_staged_files() -> u64 {
    100
}
fn d_staging_bytes() -> u64 {
    100 * 1024 * 1024
}
fn d_snapshot_files() -> u64 {
    5000
}
fn d_snapshot_bytes() -> u64 {
    500 * 1024 * 1024
}

impl Policy {
    /// Proposed editable defaults from the spec (not limits that come from Windows).
    pub fn staged_project_v1() -> Self {
        Self {
            profile: DEFAULT_PROFILE.into(),
            operations: OPERATIONS.iter().map(|o| (o.to_string(), Decision::Allow)).collect(),
            deny_paths: vec![".git".into(), ".env".into(), ".env.*".into(), "*.pem".into(), "*.key".into(), "id_rsa".into(), "id_ed25519".into()],
            max_actions: 1000,
            max_file_bytes: 10 * 1024 * 1024,
            max_session_minutes: 30,
            approval_minutes: 15,
            max_staged_files: d_staged_files(),
            max_staging_bytes: d_staging_bytes(),
            max_snapshot_files: d_snapshot_files(),
            max_snapshot_bytes: d_snapshot_bytes(),
        }
    }

    pub fn hash(&self) -> String {
        hash_value(&serde_json::to_value(self).unwrap_or(Value::Null))
    }

    /// Validates arguments strictly, then decides. Validation errors are returned as errors
    /// (nothing is recorded as an action); a valid request always gets a decision and a reason.
    pub fn evaluate(&self, op: &str, args: &Value) -> Result<(Decision, String)> {
        let parsed = parse_args(op, args, self.max_file_bytes)?;
        if let Some(p) = &parsed.path {
            if let Some(rule) = self.denied_by(p) {
                return Ok((Decision::Deny, format!("\"{p}\" matches the protected name \"{rule}\".")));
            }
        }
        match self.operations.get(op).copied().unwrap_or(Decision::Deny) {
            Decision::Allow => Ok((Decision::Allow, format!("{op} is allowed by profile {}.", self.profile))),
            Decision::RequireApproval => Ok((Decision::RequireApproval, format!("{op} needs your approval under profile {}.", self.profile))),
            Decision::Deny => Ok((Decision::Deny, format!("{op} is not allowed by profile {}.", self.profile))),
        }
    }

    fn denied_by(&self, path: &str) -> Option<String> {
        let lower = path.to_lowercase();
        for comp in lower.split('/') {
            for rule in &self.deny_paths {
                let r = rule.to_lowercase();
                let hit = if let Some(ext) = r.strip_prefix('*') {
                    comp.ends_with(ext)
                } else if let Some(prefix) = r.strip_suffix('*') {
                    comp.starts_with(prefix)
                } else {
                    comp == r
                };
                if hit {
                    return Some(rule.clone());
                }
            }
        }
        None
    }
}

pub struct ParsedArgs {
    pub path: Option<String>,
}

fn obj<'a>(args: &'a Value, allowed: &[&str], required: &[&str]) -> Result<&'a Map<String, Value>> {
    let m = args.as_object().ok_or_else(|| ProtectError::new(Code::InvalidArguments, "arguments must be an object"))?;
    if let Some(k) = m.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(ProtectError::new(Code::InvalidArguments, format!("unknown argument \"{k}\"")));
    }
    if let Some(k) = required.iter().find(|k| !m.contains_key(**k)) {
        return Err(ProtectError::new(Code::InvalidArguments, format!("missing argument \"{k}\"")));
    }
    Ok(m)
}

fn str_arg(m: &Map<String, Value>, k: &str) -> Result<Option<String>> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ProtectError::new(Code::InvalidArguments, format!("\"{k}\" must be text"))),
    }
}

pub fn parse_args(op: &str, args: &Value, max_file_bytes: u64) -> Result<ParsedArgs> {
    let path = match op {
        "files.list" | "files.diff" => str_arg(obj(args, &["path"], &[])?, "path")?.unwrap_or_default(),
        "files.read" => str_arg(obj(args, &["path"], &["path"])?, "path")?.unwrap_or_default(),
        "files.write_staged" => {
            let m = obj(args, &["path", "text"], &["path", "text"])?;
            let text = str_arg(m, "text")?.unwrap_or_default();
            if text.len() as u64 > max_file_bytes {
                return Err(ProtectError::new(Code::InvalidArguments, format!("text is larger than the {max_file_bytes}-byte limit")));
            }
            let p = str_arg(m, "path")?.unwrap_or_default();
            if p.is_empty() {
                return Err(ProtectError::new(Code::InvalidArguments, "path can't be empty for a write"));
            }
            p
        }
        _ => return Err(ProtectError::new(Code::UnsupportedOperation, format!("\"{op}\" isn't a registered operation"))),
    };
    Ok(ParsedArgs { path: Some(check_rel_path(&path)?) })
}

const DEVICE_NAMES: &[&str] = &["con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "conin$", "conout$"];

/// Accepts only plain relative paths inside the granted folder. Rejects traversal, absolute and
/// drive paths, UNC and device paths, alternate data streams and Windows-ambiguous names.
/// Returns the path with '/' separators. (Link/reparse checks need the filesystem and belong to
/// the worker, which doesn't exist yet.)
pub fn check_rel_path(p: &str) -> Result<String> {
    let bad = |why: &str| Err(ProtectError::new(Code::InvalidArguments, format!("path \"{p}\" refused: {why}")));
    if p.contains('\0') {
        return bad("contains a NUL character");
    }
    if p.contains(':') {
        return bad("drive letters and alternate data streams aren't allowed");
    }
    let s = p.replace('\\', "/");
    if s.starts_with('/') {
        return bad("absolute, UNC and device paths aren't allowed");
    }
    let mut out = vec![];
    for c in s.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            return bad("\"..\" isn't allowed");
        }
        if c.ends_with('.') || c.ends_with(' ') {
            return bad("names ending in a dot or space are ambiguous on Windows");
        }
        let stem = c.split('.').next().unwrap_or("").to_lowercase();
        if DEVICE_NAMES.contains(&stem.as_str()) {
            return bad("Windows device names aren't allowed");
        }
        if c.chars().any(|ch| matches!(ch, '<' | '>' | '"' | '|' | '?' | '*') || (ch as u32) < 32) {
            return bad("contains characters Windows doesn't allow");
        }
        out.push(c);
    }
    Ok(out.join("/"))
}

/// Canonical JSON (sorted keys) so the same arguments always hash the same.
pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<&String, Value> = m.iter().map(|(k, v)| (k, canonical(v))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        _ => v.clone(),
    }
}

pub fn hash_value(v: &Value) -> String {
    hex::encode(Sha256::digest(canonical(v).to_string().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_are_strict() {
        assert_eq!(check_rel_path("src\\a.txt").unwrap(), "src/a.txt");
        assert_eq!(check_rel_path("./a/./b").unwrap(), "a/b");
        for bad in ["../x", "a/../../x", "C:\\x", "\\\\server\\share\\x", "/etc/passwd", "a.txt:secret", "NUL", "con.txt", "dir/aux.log", "a.", "a ", "a?b", "x\u{0}y", "\\\\.\\pipe\\x", "\\\\?\\C:\\x"] {
            assert!(check_rel_path(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn decisions() {
        let mut p = Policy::staged_project_v1();
        assert_eq!(p.evaluate("files.read", &json!({"path": "notes.txt"})).unwrap().0, Decision::Allow);
        assert_eq!(p.evaluate("files.read", &json!({"path": "sub/.git/config"})).unwrap().0, Decision::Deny);
        assert_eq!(p.evaluate("files.write_staged", &json!({"path": "keys/server.PEM", "text": ""})).unwrap().0, Decision::Deny);
        assert_eq!(p.evaluate("files.read", &json!({"path": ".env.local"})).unwrap().0, Decision::Deny);
        assert_eq!(p.evaluate("shell.run", &json!({})).unwrap_err().code, Code::UnsupportedOperation);
        assert_eq!(p.evaluate("files.read", &json!({"path": "a", "mode": "x"})).unwrap_err().code, Code::InvalidArguments);
        assert_eq!(p.evaluate("files.write_staged", &json!({"path": "a"})).unwrap_err().code, Code::InvalidArguments);
        p.operations.insert("files.write_staged".into(), Decision::RequireApproval);
        assert_eq!(p.evaluate("files.write_staged", &json!({"path": "a.txt", "text": "hi"})).unwrap().0, Decision::RequireApproval);
        p.max_file_bytes = 3;
        assert!(p.evaluate("files.write_staged", &json!({"path": "a.txt", "text": "four"})).is_err());
    }

    #[test]
    fn hashing_ignores_key_order() {
        assert_eq!(hash_value(&json!({"a": 1, "b": {"y": 2, "x": 1}})), hash_value(&json!({"b": {"x": 1, "y": 2}, "a": 1})));
        assert_ne!(hash_value(&json!({"a": 1})), hash_value(&json!({"a": 2})));
    }
}
