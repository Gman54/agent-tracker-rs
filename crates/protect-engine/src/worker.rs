//! The staged file worker: the only thing in Protect that actually *does* work.
//!
//! It is deliberately small and runs no code of its own. For a session it keeps two folders
//! under the engine's own data directory:
//!
//!   sessions/<session-id>/snapshot   a read-only copy of the granted folder, taken once
//!   sessions/<session-id>/staging    where write_staged puts files
//!
//! The worker only ever reads from the snapshot and writes into staging. It never opens the
//! real granted folder again after the snapshot, so the user's originals cannot be written or
//! deleted by anything a session does. This is a confined *file* worker for four typed
//! operations, not an OS process sandbox: there is no arbitrary code to isolate because the
//! profile has no shell, no code execution and no network.
use crate::errors::{Code, ProtectError, Result};
use crate::policy::{self, Policy};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn err(code: Code, msg: impl Into<String>) -> ProtectError {
    ProtectError::new(code, msg)
}

/// The per-session workspace. `root` is `<data_dir>/sessions/<session_id>`.
pub struct Workspace {
    root: PathBuf,
}

/// What the snapshot ended up containing, for the session.ready event.
pub struct SnapshotStats {
    pub files: u64,
    pub bytes: u64,
    pub skipped: u64,
}

impl Workspace {
    pub fn new(data_dir: &Path, session_id: &str) -> Self {
        Self { root: data_dir.join("sessions").join(session_id) }
    }

    fn snapshot(&self) -> PathBuf {
        self.root.join("snapshot")
    }

    fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// Takes the one-time read-only snapshot of `source` and creates an empty staging folder.
    /// Follows no symlinks, skips anything the policy's deny list covers, and stops with an
    /// error if the folder is bigger than the snapshot caps (so a session can't copy a huge
    /// tree into the engine's data folder).
    pub fn prepare(&self, source: &Path, policy: &Policy) -> Result<SnapshotStats> {
        let snap = self.snapshot();
        std::fs::create_dir_all(&snap).map_err(|e| err(Code::AuditUnavailable, format!("couldn't make the snapshot folder: {e}")))?;
        std::fs::create_dir_all(self.staging()).map_err(|e| err(Code::AuditUnavailable, format!("couldn't make the staging folder: {e}")))?;
        let mut stats = SnapshotStats { files: 0, bytes: 0, skipped: 0 };
        copy_tree(source, &snap, "", policy, &mut stats)?;
        Ok(stats)
    }

    /// Runs one typed operation and returns its result. Read operations read the snapshot
    /// (overlaid with anything already staged); write_staged writes only into staging.
    pub fn run(&self, op: &str, args: &Value, policy: &Policy) -> Result<Value> {
        let parsed = policy::parse_args(op, args, policy.max_file_bytes)?;
        let rel = parsed.path.unwrap_or_default();
        match op {
            "files.list" => self.list(&rel),
            "files.read" => self.read(&rel, policy),
            "files.diff" => self.diff(&rel),
            "files.write_staged" => {
                let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
                self.write_staged(&rel, text, policy)
            }
            _ => Err(err(Code::UnsupportedOperation, format!("the worker doesn't run \"{op}\""))),
        }
    }

    fn list(&self, rel: &str) -> Result<Value> {
        // Merge the snapshot (original files) with staging (changed/new files).
        let mut seen: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
        for (base, source) in [(self.snapshot(), "original"), (self.staging(), "staged")] {
            let start = if rel.is_empty() { base.clone() } else { safe_join(&base, rel)? };
            for (p, bytes) in walk_files(&start, &base) {
                seen.insert(p.clone(), json!({"path": p, "bytes": bytes, "source": source}));
            }
        }
        Ok(json!({"files": seen.into_values().collect::<Vec<_>>()}))
    }

    fn read(&self, rel: &str, policy: &Policy) -> Result<Value> {
        if rel.is_empty() {
            return Err(err(Code::InvalidArguments, "a file path is needed to read"));
        }
        // Prefer a staged version if the session already wrote one.
        let staged = safe_join(&self.staging(), rel)?;
        let path = if staged.is_file() { staged } else { safe_join(&self.snapshot(), rel)? };
        if !path.is_file() {
            return Err(err(Code::NotFound, format!("\"{rel}\" isn't in the granted folder")));
        }
        let bytes = std::fs::read(&path).map_err(|e| err(Code::AuditUnavailable, format!("couldn't read \"{rel}\": {e}")))?;
        if bytes.len() as u64 > policy.max_file_bytes {
            return Err(err(Code::InvalidArguments, format!("\"{rel}\" is larger than the {}-byte read limit", policy.max_file_bytes)));
        }
        match String::from_utf8(bytes.clone()) {
            Ok(text) => Ok(json!({"path": rel, "bytes": text.len(), "encoding": "utf-8", "text": text})),
            Err(_) => Ok(json!({"path": rel, "bytes": bytes.len(), "encoding": "base64", "base64": base64_std(&bytes)})),
        }
    }

    fn write_staged(&self, rel: &str, text: &str, policy: &Policy) -> Result<Value> {
        let target = safe_join(&self.staging(), rel)?;
        // Enforce the staging caps across everything already staged, counting this write.
        let (mut count, mut total) = (0u64, 0u64);
        for (p, bytes) in walk_files(&self.staging(), &self.staging()) {
            if p != rel {
                count += 1;
                total += bytes;
            }
        }
        count += 1;
        total += text.len() as u64;
        if count > policy.max_staged_files {
            return Err(err(Code::PolicyDenied, format!("this would stage {count} files, over the limit of {}", policy.max_staged_files)));
        }
        if total > policy.max_staging_bytes {
            return Err(err(Code::PolicyDenied, format!("this would use {total} bytes of staging, over the limit of {}", policy.max_staging_bytes)));
        }
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| err(Code::AuditUnavailable, format!("couldn't make the staging path: {e}")))?;
        }
        std::fs::write(&target, text.as_bytes()).map_err(|e| err(Code::AuditUnavailable, format!("couldn't write the staged file: {e}")))?;
        Ok(json!({"path": rel, "bytes_written": text.len(), "staged": true, "wrote_original": false}))
    }

    fn diff(&self, rel: &str) -> Result<Value> {
        let mut changes = vec![];
        let staging = self.staging();
        let start = if rel.is_empty() { staging.clone() } else { safe_join(&staging, rel)? };
        for (p, new_bytes) in walk_files(&start, &staging) {
            let orig = safe_join(&self.snapshot(), &p)?;
            let (kind, old_bytes) = match std::fs::metadata(&orig) {
                Ok(m) => ("modified", m.len()),
                Err(_) => ("added", 0),
            };
            changes.push(json!({"path": p, "kind": kind, "old_bytes": old_bytes, "new_bytes": new_bytes}));
        }
        changes.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(json!({"changes": changes, "note": "The worker never deletes or writes your original files; these are staged changes only."}))
    }

    /// The staged files' relative paths, sorted.
    pub fn staged_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = walk_files(&self.staging(), &self.staging()).into_iter().map(|(p, _)| p).collect();
        v.sort();
        v
    }

    /// For the person reviewing: every staged change with the old and new text (each capped at
    /// `max_text` bytes) and whether the real file changed since the snapshot was taken.
    /// Only the user's own Protect controls call this; it is not an operation a session can submit.
    pub fn review(&self, real_root: &Path, max_text: u64) -> Result<Value> {
        let mut out = vec![];
        for rel in self.staged_paths() {
            let snap = safe_join(&self.snapshot(), &rel)?;
            let staged = safe_join(&self.staging(), &rel)?;
            let kind = if snap.is_file() { "modified" } else { "added" };
            let (old_text, old_note) = text_for_review(&snap, max_text);
            let (new_text, new_note) = text_for_review(&staged, max_text);
            let stale = !self.original_unchanged(real_root, &rel)?;
            out.push(json!({"path": rel, "kind": kind, "old_text": old_text, "new_text": new_text, "old_note": old_note, "new_note": new_note,
                "original_changed_since_snapshot": stale}));
        }
        Ok(json!({"changes": out}))
    }

    /// True when the real file still matches what the snapshot saw: same bytes, or, for a file
    /// the session added, still absent. Anything else means someone changed it in the meantime.
    fn original_unchanged(&self, real_root: &Path, rel: &str) -> Result<bool> {
        let snap = safe_join(&self.snapshot(), rel)?;
        let real = safe_join(real_root, rel)?;
        if snap.is_file() {
            match (std::fs::read(&snap), std::fs::read(&real)) {
                (Ok(a), Ok(b)) => Ok(a == b),
                _ => Ok(false),
            }
        } else {
            Ok(std::fs::symlink_metadata(&real).is_err())
        }
    }

    /// Writes one staged file into the real folder, after the user said yes. Refuses if the real
    /// file changed since the snapshot (so nobody's newer edit gets overwritten) or if the policy's
    /// deny list covers the path. Writes to a temporary file first and renames it into place. On
    /// success the snapshot is brought up to date and the staged copy is removed.
    pub fn apply(&self, real_root: &Path, rel: &str, policy: &Policy) -> Result<Value> {
        let staged = safe_join(&self.staging(), rel)?;
        if !staged.is_file() {
            return Err(err(Code::NotFound, format!("there's no staged change for \"{rel}\"")));
        }
        if policy_denies(policy, rel) {
            return Err(err(Code::PolicyDenied, format!("\"{rel}\" is on Protect's do-not-touch list")));
        }
        if !self.original_unchanged(real_root, rel)? {
            return Err(err(Code::StaleResource, format!("\"{rel}\" was changed in your folder after the helper started, so it was left alone")));
        }
        let kind = if safe_join(&self.snapshot(), rel)?.is_file() { "modified" } else { "added" };
        let bytes = std::fs::read(&staged).map_err(|e| err(Code::AuditUnavailable, format!("couldn't read the staged file: {e}")))?;
        let real = safe_join(real_root, rel)?;
        if let Some(dir) = real.parent() {
            std::fs::create_dir_all(dir).map_err(|e| err(Code::AuditUnavailable, format!("couldn't make the folder for \"{rel}\": {e}")))?;
        }
        let tmp = real.with_file_name(format!(".{}.houndbit-tmp", real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        std::fs::write(&tmp, &bytes).map_err(|e| err(Code::AuditUnavailable, format!("couldn't write \"{rel}\": {e}")))?;
        if let Err(e) = std::fs::rename(&tmp, &real) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err(Code::AuditUnavailable, format!("couldn't put \"{rel}\" in place: {e}")));
        }
        let snap = safe_join(&self.snapshot(), rel)?;
        if let Some(dir) = snap.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&snap, &bytes);
        let _ = std::fs::remove_file(&staged);
        Ok(json!({"path": rel, "kind": kind, "bytes_written": bytes.len(), "wrote_original": true}))
    }

    /// Throws away one staged change. The real folder is not touched.
    pub fn discard(&self, rel: &str) -> Result<()> {
        let staged = safe_join(&self.staging(), rel)?;
        if !staged.is_file() {
            return Err(err(Code::NotFound, format!("there's no staged change for \"{rel}\"")));
        }
        std::fs::remove_file(&staged).map_err(|e| err(Code::AuditUnavailable, format!("couldn't remove the staged file: {e}")))
    }
}

/// Text to show in a review, or None with a short note when it isn't shown.
fn text_for_review(path: &Path, max: u64) -> (Option<String>, Option<&'static str>) {
    match std::fs::read(path) {
        Err(_) => (None, None),
        Ok(b) if b.len() as u64 > max => (None, Some("too large to show here")),
        Ok(b) => match String::from_utf8(b) {
            Ok(t) => (Some(t), None),
            Err(_) => (None, Some("not a text file")),
        },
    }
}

/// Joins a relative path under `base`, refusing any component that is a symbolic link or
/// reparse point. The policy already rejected `..`, drive letters and the like at decision
/// time; this guards against a link that was swapped in between the decision and the use.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let mut p = base.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(err(Code::InvalidArguments, "path tried to leave the folder"));
        }
        p.push(comp);
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            if meta.file_type().is_symlink() {
                return Err(err(Code::InvalidArguments, format!("\"{rel}\" goes through a link, which isn't allowed")));
            }
        }
    }
    Ok(p)
}

/// Copies regular files from `src` into `dst`, skipping symlinks and policy-denied names, with
/// '/'-joined relative paths. Caps both file count and total bytes.
fn copy_tree(src: &Path, dst_root: &Path, rel: &str, policy: &Policy, stats: &mut SnapshotStats) -> Result<()> {
    let dir = if rel.is_empty() { src.to_path_buf() } else { src.join(rel) };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Never follow links into the snapshot: they could point outside the granted folder.
        if meta.file_type().is_symlink() {
            stats.skipped += 1;
            continue;
        }
        if policy_denies(policy, &child_rel) {
            stats.skipped += 1;
            continue;
        }
        if meta.is_dir() {
            copy_tree(src, dst_root, &child_rel, policy, stats)?;
        } else if meta.is_file() {
            if stats.files + 1 > policy.max_snapshot_files {
                return Err(err(Code::PolicyDenied, format!("the folder has more than the {} files the snapshot allows", policy.max_snapshot_files)));
            }
            if stats.bytes + meta.len() > policy.max_snapshot_bytes {
                return Err(err(Code::PolicyDenied, format!("the folder is bigger than the {}-byte snapshot limit", policy.max_snapshot_bytes)));
            }
            let out = dst_root.join(&child_rel);
            if let Some(d) = out.parent() {
                std::fs::create_dir_all(d).map_err(|e| err(Code::AuditUnavailable, format!("couldn't build the snapshot: {e}")))?;
            }
            std::fs::copy(entry.path(), &out).map_err(|e| err(Code::AuditUnavailable, format!("couldn't copy into the snapshot: {e}")))?;
            stats.files += 1;
            stats.bytes += meta.len();
        }
    }
    Ok(())
}

/// Lists regular files under `start`, returning their paths relative to `base`.
fn walk_files(start: &Path, base: &Path) -> Vec<(String, u64)> {
    let mut out = vec![];
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                if let Ok(rel) = entry.path().strip_prefix(base) {
                    out.push((rel.to_string_lossy().replace('\\', "/"), meta.len()));
                }
            }
        }
    }
    out
}

fn policy_denies(policy: &Policy, rel: &str) -> bool {
    // Reuse the policy's own name matching by asking it about a read of this path.
    policy.evaluate("files.read", &json!({"path": rel})).map(|(d, _)| d == policy::Decision::Deny).unwrap_or(true)
}

fn base64_std(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, Workspace, Policy) {
        let data = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("a.txt"), "alpha").unwrap();
        std::fs::create_dir_all(source.path().join("sub")).unwrap();
        std::fs::write(source.path().join("sub/b.txt"), "beta").unwrap();
        std::fs::create_dir_all(source.path().join(".git")).unwrap();
        std::fs::write(source.path().join(".git/config"), "secret").unwrap();
        let ws = Workspace::new(data.path(), "ses_test");
        let policy = Policy::staged_project_v1();
        ws.prepare(source.path(), &policy).unwrap();
        (data, source, ws, policy)
    }

    #[test]
    fn reads_originals_and_skips_secrets() {
        let (_d, _s, ws, p) = setup();
        let r = ws.read("a.txt", &p).unwrap();
        assert_eq!(r["text"], "alpha");
        // .git was excluded from the snapshot, so it reads as not found.
        assert_eq!(ws.read(".git/config", &p).unwrap_err().code, Code::NotFound);
        let list = ws.list("").unwrap();
        let paths: Vec<&str> = list["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"a.txt") && paths.contains(&"sub/b.txt"));
        assert!(!paths.iter().any(|p| p.contains(".git")));
    }

    #[test]
    fn write_staged_never_touches_the_original() {
        let (_d, source, ws, p) = setup();
        ws.write_staged("a.txt", "changed", &p).unwrap();
        // The real file in the granted folder is untouched.
        assert_eq!(std::fs::read_to_string(source.path().join("a.txt")).unwrap(), "alpha");
        // A later read sees the staged version.
        assert_eq!(ws.read("a.txt", &p).unwrap()["text"], "changed");
        let diff = ws.diff("").unwrap();
        let changes = diff["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!((changes[0]["path"].as_str(), changes[0]["kind"].as_str()), (Some("a.txt"), Some("modified")));
    }

    #[test]
    fn staging_caps_are_enforced() {
        let (_d, _s, ws, mut p) = setup();
        p.max_staged_files = 1;
        ws.write_staged("one.txt", "x", &p).unwrap();
        assert_eq!(ws.write_staged("two.txt", "y", &p).unwrap_err().code, Code::PolicyDenied);
        // Re-writing the same staged file stays within the one-file limit.
        ws.write_staged("one.txt", "xx", &p).unwrap();
    }

    #[test]
    fn apply_writes_the_original_only_when_it_is_unchanged() {
        let (_d, source, ws, p) = setup();
        ws.write_staged("a.txt", "changed", &p).unwrap();
        ws.write_staged("new/c.txt", "brand new", &p).unwrap();
        ws.write_staged("sub/b.txt", "beta 2", &p).unwrap();
        let rev = ws.review(source.path(), 1000).unwrap();
        assert_eq!(rev["changes"].as_array().unwrap().len(), 3);
        // Someone edits b.txt in the real folder after the snapshot.
        std::fs::write(source.path().join("sub/b.txt"), "edited by hand").unwrap();
        let rev = ws.review(source.path(), 1000).unwrap();
        let b = rev["changes"].as_array().unwrap().iter().find(|c| c["path"] == "sub/b.txt").unwrap().clone();
        assert_eq!(b["original_changed_since_snapshot"], true);
        assert_eq!(b["old_text"], "beta");
        assert_eq!(b["new_text"], "beta 2");

        ws.apply(source.path(), "a.txt", &p).unwrap();
        ws.apply(source.path(), "new/c.txt", &p).unwrap();
        assert_eq!(std::fs::read_to_string(source.path().join("a.txt")).unwrap(), "changed");
        assert_eq!(std::fs::read_to_string(source.path().join("new/c.txt")).unwrap(), "brand new");
        assert_eq!(ws.apply(source.path(), "sub/b.txt", &p).unwrap_err().code, Code::StaleResource);
        assert_eq!(std::fs::read_to_string(source.path().join("sub/b.txt")).unwrap(), "edited by hand");
        // Applied changes leave staging; the refused one stays for the person to decide.
        assert_eq!(ws.staged_paths(), vec!["sub/b.txt".to_string()]);
        ws.discard("sub/b.txt").unwrap();
        assert!(ws.staged_paths().is_empty());
        assert_eq!(std::fs::read_to_string(source.path().join("sub/b.txt")).unwrap(), "edited by hand");
    }

    #[test]
    fn apply_refuses_an_added_file_that_now_exists() {
        let (_d, source, ws, p) = setup();
        ws.write_staged("fresh.txt", "mine", &p).unwrap();
        std::fs::write(source.path().join("fresh.txt"), "someone else's").unwrap();
        assert_eq!(ws.apply(source.path(), "fresh.txt", &p).unwrap_err().code, Code::StaleResource);
        assert_eq!(std::fs::read_to_string(source.path().join("fresh.txt")).unwrap(), "someone else's");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_source_is_not_followed() {
        let data = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), source.path().join("link.txt")).unwrap();
        std::fs::write(source.path().join("ok.txt"), "fine").unwrap();
        let ws = Workspace::new(data.path(), "ses_link");
        let p = Policy::staged_project_v1();
        let stats = ws.prepare(source.path(), &p).unwrap();
        assert_eq!(stats.files, 1, "only ok.txt copied");
        assert_eq!(ws.read("link.txt", &p).unwrap_err().code, Code::NotFound);
        assert_eq!(ws.read("ok.txt", &p).unwrap()["text"], "fine");
    }
}
