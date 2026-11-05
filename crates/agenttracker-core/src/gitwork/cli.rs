//! Thin, read-only wrappers around the `git` command line. Nothing here changes a repository.
use crate::{CoreError, Result};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

/// Largest file we keep as text evidence.
pub const MAX_TEXT_BYTES: usize = 512 * 1024;

fn command(dir: &Path, args: &[&str]) -> std::process::Command {
    let mut c = crate::connectors::hidden_command("git");
    c.arg("-C").arg(dir).args(["-c", "core.quotepath=off", "-c", "color.ui=never"]).args(args);
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0").env("LC_ALL", "C");
    c
}

/// Runs git and returns stdout bytes when it exits successfully.
pub fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = command(dir, args).output().map_err(|e| CoreError::Invalid(format!("Couldn't run git ({e}). Is Git installed?")))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(CoreError::Invalid(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run_bytes(dir, args)?).into_owned())
}

/// Like [`run`] but trimmed, and None on any failure.
pub fn try_line(dir: &Path, args: &[&str]) -> Option<String> {
    run(dir, args).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Walks up from `path` looking for a `.git` folder or file, without starting git.
pub fn find_repo_root(path: &Path) -> Option<PathBuf> {
    let mut cur = if path.is_dir() { Some(path) } else { path.parent() };
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Git's blob id for these bytes (same as `git hash-object`).
pub fn blob_id(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Text form of file bytes for evidence: None for binary or oversized content.
pub fn as_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_TEXT_BYTES || bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// Scrubs secrets from text with the same rules used for event payloads.
pub fn scrub(text: &str) -> String {
    match tracker_common::redaction::redact_deep(&serde_json::Value::String(text.to_string())) {
        serde_json::Value::String(s) => s,
        _ => text.to_string(),
    }
}

/// Normalizes a remote URL so https and ssh forms of the same repository compare equal.
pub fn normalize_remote(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').trim_end_matches(".git").to_string();
    for p in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = u.strip_prefix(p) {
            u = rest.to_string();
        }
    }
    if let Some((_, rest)) = u.split_once('@') {
        u = rest.to_string();
    }
    // git@github.com:owner/repo → github.com/owner/repo
    if let Some((host, path)) = u.split_once(':') {
        if !path.starts_with("//") && !host.contains('/') {
            u = format!("{host}/{}", path.trim_start_matches('/'));
        }
    }
    u.to_ascii_lowercase()
}

/// owner/repo for a github.com remote.
pub fn github_slug(url: &str) -> Option<(String, String)> {
    let n = normalize_remote(url);
    let rest = n.strip_prefix("github.com/")?;
    let mut parts = rest.split('/');
    let (o, r) = (parts.next()?, parts.next()?);
    (!o.is_empty() && !r.is_empty()).then(|| (o.to_string(), r.to_string()))
}

/// Stable identity for a repository: its normalized "origin" (or first) remote, otherwise its
/// first commit. Returns (identity, remote url).
pub fn repo_identity(root: &Path) -> (String, Option<String>) {
    let remote = try_line(root, &["remote", "get-url", "origin"]).or_else(|| {
        let first = try_line(root, &["remote"])?.lines().next()?.to_string();
        try_line(root, &["remote", "get-url", &first])
    });
    if let Some(r) = &remote {
        return (normalize_remote(r), remote.clone());
    }
    let first = try_line(root, &["rev-list", "--max-parents=0", "HEAD"]).and_then(|s| s.lines().last().map(str::to_string));
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    (first.map(|f| format!("local:{f}")).unwrap_or_else(|| format!("folder:{}", crate::fs_observer::normalize_path(&canon))), None)
}

/// Path of `file` relative to `root` with forward slashes, comparing case-insensitively on
/// Windows but keeping the file's own spelling.
pub fn rel_in_repo(root: &Path, file: &Path) -> Option<String> {
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").trim_start_matches("//?/").to_string();
    let canon_root = std::fs::canonicalize(root).map(|p| norm(&p)).unwrap_or_else(|_| norm(root));
    let f = norm(file);
    let canon_file = std::fs::canonicalize(file).map(|p| norm(&p)).ok();
    for r in [norm(root), canon_root] {
        let r = r.trim_end_matches('/').to_string();
        for cand in [Some(f.clone()), canon_file.clone()].into_iter().flatten() {
            let (a, b) = if cfg!(windows) { (cand.to_lowercase(), r.to_lowercase()) } else { (cand.clone(), r.clone()) };
            if a.len() > b.len() && a.starts_with(&b) && a.as_bytes()[b.len()] == b'/' {
                return Some(cand[r.len() + 1..].to_string());
            }
        }
    }
    None
}

pub fn same_root(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        let p = std::fs::canonicalize(s).unwrap_or_else(|_| PathBuf::from(s));
        let t = p.to_string_lossy().replace('\\', "/").trim_start_matches("//?/").trim_end_matches('/').to_string();
        if cfg!(windows) { t.to_lowercase() } else { t }
    };
    n(a) == n(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalize() {
        assert_eq!(normalize_remote("git@github.com:Gman54/agent-tracker-rs.git"), "github.com/gman54/agent-tracker-rs");
        assert_eq!(normalize_remote("https://github.com/Gman54/agent-tracker-rs"), "github.com/gman54/agent-tracker-rs");
        assert_eq!(normalize_remote("https://user:x@github.com/Gman54/agent-tracker-rs.git/"), "github.com/gman54/agent-tracker-rs");
        assert_eq!(github_slug("git@github.com:Gman54/Repo.git"), Some(("gman54".into(), "repo".into())));
        assert_eq!(github_slug("/srv/git/thing.git"), None);
    }

    #[test]
    fn blob_ids_match_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(blob_id(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
    }
}
