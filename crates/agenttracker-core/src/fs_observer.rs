//! Filesystem observation: live change hints (notify) and authoritative hash snapshots.
//! Change hints are only hints — verification always relies on snapshot manifests.
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const HASH_CHUNK: usize = 4 * 1024 * 1024;
pub const MAX_HASH_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    File,
    Dir,
    Symlink,
    Other,
}

impl EntryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "file" => Self::File,
            "dir" => Self::Dir,
            "symlink" => Self::Symlink,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stability {
    Stable,
    /// The file kept changing while being hashed (after MAX_HASH_ATTEMPTS); hash is not trustworthy.
    Unstable,
}

/// One manifest row (spec §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Path relative to the manifest root, '/'-separated.
    pub rel_path: String,
    pub entry_type: EntryType,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub modified_ts: Option<i64>,
    pub observed_ts: i64,
    pub read_ok: bool,
    pub error_detail: Option<String>,
    pub stability: Stability,
}

impl ManifestEntry {
    /// The hash can be trusted for comparisons.
    pub fn trustworthy(&self) -> bool {
        self.read_ok && self.stability == Stability::Stable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestType {
    Baseline,
    Verification,
}

impl ManifestType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Verification => "verification",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub task_id: Option<String>,
    /// Absolute root folder (as the user configured it, normalized).
    pub root: String,
    pub captured_at: i64,
    pub manifest_type: ManifestType,
    pub entries: Vec<ManifestEntry>,
    /// False if the root itself could not be read.
    pub root_ok: bool,
}

impl Manifest {
    pub fn get(&self, rel: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.rel_path == rel)
    }
    pub fn files(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.entries.iter().filter(|e| e.entry_type == EntryType::File)
    }
}

/// Normalizes a user path to a comparable string: absolute components, '/' separators,
/// no trailing slash, `.`/`..` resolved lexically. Lower-cased on Windows (case-insensitive FS).
pub fn normalize_path(p: &Path) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut prefix = String::new();
    for c in p.components() {
        match c {
            Component::Prefix(pre) => prefix = pre.as_os_str().to_string_lossy().replace("\\\\?\\", "").replace('\\', "/"),
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    let s = format!("{prefix}/{}", out.join("/"));
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// If `path` lies within `root` (both normalized), returns the relative part ("" for root itself).
pub fn relative_to(root: &str, path: &str) -> Option<String> {
    if path == root {
        return Some(String::new());
    }
    let r = if root.ends_with('/') { root.to_string() } else { format!("{root}/") };
    path.strip_prefix(&r).map(str::to_string)
}

/// Windows extended-length path (`\\?\C:\...`) so paths over MAX_PATH work. No-op elsewhere.
pub fn extended_path(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.as_os_str().to_string_lossy();
        if s.starts_with(r"\\?\") {
            return p.to_path_buf();
        }
        if p.is_absolute() {
            if let Some(unc) = s.strip_prefix(r"\\") {
                return PathBuf::from(format!(r"\\?\UNC\{unc}"));
            }
            return PathBuf::from(format!(r"\\?\{}", s.replace('/', "\\")));
        }
    }
    p.to_path_buf()
}

// ---------------------------------------------------------------------------------------------
// Change hints (notify)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeHint {
    pub path: String,
    pub kind: String,
    pub ts: i64,
}

pub struct Observer;

pub struct ObserverHandle {
    _watcher: RecommendedWatcher,
    hints: Arc<Mutex<Vec<ChangeHint>>>,
    errors: Arc<Mutex<Vec<String>>>,
    pub root: PathBuf,
}

impl Observer {
    /// Starts watching `root` recursively. Events are buffered until drained.
    pub fn start(root: PathBuf) -> notify::Result<ObserverHandle> {
        let hints = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (h, e) = (hints.clone(), errors.clone());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                let kind = format!("{:?}", ev.kind);
                let mut g = h.lock().unwrap_or_else(|p| p.into_inner());
                for p in ev.paths {
                    if g.len() < 100_000 {
                        g.push(ChangeHint { path: normalize_path(&p), kind: kind.clone(), ts: crate::now_ms() });
                    }
                }
            }
            Err(err) => e.lock().unwrap_or_else(|p| p.into_inner()).push(err.to_string()),
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        Ok(ObserverHandle { _watcher: watcher, hints, errors, root })
    }
}

impl ObserverHandle {
    pub fn drain(&self) -> Vec<ChangeHint> {
        std::mem::take(&mut *self.hints.lock().unwrap_or_else(|p| p.into_inner()))
    }
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Snapshotter
// ---------------------------------------------------------------------------------------------

type HashHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Captures hash manifests. `after_hash_hook` is an instrumentation point invoked after each file
/// is hashed and before the post-hash stat (used by tests to simulate files changing mid-capture).
#[derive(Clone, Default)]
pub struct Snapshotter {
    pub after_hash_hook: Option<HashHook>,
}

fn mtime_ms(md: &fs::Metadata) -> Option<i64> {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64)
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

impl Snapshotter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience wrapper matching the spec signature.
    pub fn capture(root: &Path, exclude_paths: &[PathBuf]) -> Manifest {
        Self::new().capture_with(root, exclude_paths, ManifestType::Baseline, None)
    }

    pub fn capture_with(&self, root: &Path, exclude_paths: &[PathBuf], manifest_type: ManifestType, task_id: Option<&str>) -> Manifest {
        let mut m = Manifest {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.map(str::to_string),
            root: normalize_path(root),
            captured_at: crate::now_ms(),
            manifest_type,
            entries: Vec::new(),
            root_ok: true,
        };
        let canon_root = match fs::canonicalize(extended_path(root)) {
            Ok(c) => c,
            Err(e) => {
                m.root_ok = false;
                m.entries.push(ManifestEntry {
                    rel_path: String::new(),
                    entry_type: EntryType::Dir,
                    size_bytes: None,
                    sha256: None,
                    modified_ts: None,
                    observed_ts: crate::now_ms(),
                    read_ok: false,
                    error_detail: Some(format!("cannot open folder: {e}")),
                    stability: Stability::Stable,
                });
                return m;
            }
        };
        let excludes: Vec<PathBuf> = exclude_paths.iter().map(|p| fs::canonicalize(extended_path(p)).unwrap_or_else(|_| p.clone())).collect();
        let mut stack: Vec<(PathBuf, String)> = vec![(canon_root.clone(), String::new())];
        while let Some((dir, rel_dir)) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    if let Some(entry) = m.entries.iter_mut().find(|x| x.rel_path == rel_dir) {
                        entry.read_ok = false;
                        entry.error_detail = Some(format!("cannot list folder: {e}"));
                    } else {
                        m.root_ok = false;
                    }
                    continue;
                }
            };
            let mut children: Vec<_> = rd.filter_map(|r| r.ok()).collect();
            children.sort_by_key(|d| d.file_name());
            for child in children {
                let path = child.path();
                if excludes.iter().any(|x| path.starts_with(x)) {
                    continue;
                }
                let name = child.file_name().to_string_lossy().into_owned();
                let rel = if rel_dir.is_empty() { name } else { format!("{rel_dir}/{name}") };
                let observed_ts = crate::now_ms();
                let md = match fs::symlink_metadata(&path) {
                    Ok(md) => md,
                    Err(e) => {
                        m.entries.push(err_entry(&rel, EntryType::Other, observed_ts, format!("cannot stat: {e}")));
                        continue;
                    }
                };
                let ft = md.file_type();
                // Reparse check limited to folders: OneDrive placeholder *files* are reparse points too.
                if ft.is_symlink() || (ft.is_dir() && is_reparse_point(&md)) {
                    // Never traverse links/junctions; record where they point and whether that is inside the root.
                    let detail = match fs::read_link(&path).ok().and_then(|t| {
                        let abs = if t.is_absolute() { t } else { path.parent().map(|p| p.join(&t)).unwrap_or(t) };
                        fs::canonicalize(abs).ok()
                    }) {
                        Some(target) if target.starts_with(&canon_root) => "link inside root; not traversed (target captured directly)".to_string(),
                        Some(_) => "link points outside the folder; not traversed".to_string(),
                        None => "link target unavailable; not traversed".to_string(),
                    };
                    m.entries.push(ManifestEntry {
                        rel_path: rel,
                        entry_type: EntryType::Symlink,
                        size_bytes: None,
                        sha256: None,
                        modified_ts: mtime_ms(&md),
                        observed_ts,
                        read_ok: true,
                        error_detail: Some(detail),
                        stability: Stability::Stable,
                    });
                    continue;
                }
                if ft.is_dir() {
                    // Defence in depth: a directory whose canonical path escapes the root is skipped.
                    match fs::canonicalize(&path) {
                        Ok(c) if c.starts_with(&canon_root) => {
                            m.entries.push(ManifestEntry {
                                rel_path: rel.clone(),
                                entry_type: EntryType::Dir,
                                size_bytes: None,
                                sha256: None,
                                modified_ts: mtime_ms(&md),
                                observed_ts,
                                read_ok: true,
                                error_detail: None,
                                stability: Stability::Stable,
                            });
                            stack.push((path, rel));
                        }
                        _ => m.entries.push(err_entry(&rel, EntryType::Symlink, observed_ts, "folder resolves outside root; not traversed".into())),
                    }
                    continue;
                }
                if ft.is_file() {
                    m.entries.push(self.hash_entry(&path, &rel, &md, observed_ts));
                } else {
                    m.entries.push(ManifestEntry {
                        rel_path: rel,
                        entry_type: EntryType::Other,
                        size_bytes: Some(md.len()),
                        sha256: None,
                        modified_ts: mtime_ms(&md),
                        observed_ts,
                        read_ok: false,
                        error_detail: Some("special file; not hashed".into()),
                        stability: Stability::Stable,
                    });
                }
            }
        }
        m.entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        m
    }

    fn hash_entry(&self, path: &Path, rel: &str, md0: &fs::Metadata, observed_ts: i64) -> ManifestEntry {
        let mut before = (md0.len(), mtime_ms(md0));
        let mut last_err: Option<String> = None;
        for attempt in 1..=MAX_HASH_ATTEMPTS {
            match hash_file(path) {
                Ok(h) => {
                    if let Some(hook) = &self.after_hash_hook {
                        hook(path);
                    }
                    match fs::metadata(path) {
                        Ok(md1) => {
                            let after = (md1.len(), mtime_ms(&md1));
                            if after == before {
                                return ManifestEntry {
                                    rel_path: rel.to_string(),
                                    entry_type: EntryType::File,
                                    size_bytes: Some(after.0),
                                    sha256: Some(h),
                                    modified_ts: after.1,
                                    observed_ts,
                                    read_ok: true,
                                    error_detail: None,
                                    stability: Stability::Stable,
                                };
                            }
                            last_err = Some(format!("file changed while hashing (attempt {attempt})"));
                            before = after;
                        }
                        Err(e) => return err_entry(rel, EntryType::File, observed_ts, format!("file vanished while hashing: {e}")),
                    }
                }
                Err(e) => {
                    return ManifestEntry {
                        rel_path: rel.to_string(),
                        entry_type: EntryType::File,
                        size_bytes: Some(md0.len()),
                        sha256: None,
                        modified_ts: mtime_ms(md0),
                        observed_ts,
                        read_ok: false,
                        error_detail: Some(format!("cannot read (locked or no permission): {e}")),
                        stability: Stability::Stable,
                    }
                }
            }
        }
        ManifestEntry {
            rel_path: rel.to_string(),
            entry_type: EntryType::File,
            size_bytes: Some(before.0),
            sha256: None,
            modified_ts: before.1,
            observed_ts,
            read_ok: true,
            error_detail: Some(format!("{} — gave up after {MAX_HASH_ATTEMPTS} attempts", last_err.unwrap_or_default())),
            stability: Stability::Unstable,
        }
    }
}

fn err_entry(rel: &str, t: EntryType, observed_ts: i64, detail: String) -> ManifestEntry {
    ManifestEntry {
        rel_path: rel.to_string(),
        entry_type: t,
        size_bytes: None,
        sha256: None,
        modified_ts: None,
        observed_ts,
        read_ok: false,
        error_detail: Some(detail),
        stability: Stability::Stable,
    }
}

#[cfg(windows)]
fn is_reparse_point(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_md: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_paths() {
        assert_eq!(relative_to("/a/b", "/a/b/c/d"), Some("c/d".into()));
        assert_eq!(relative_to("/a/b", "/a/bc"), None);
        assert_eq!(relative_to("/a/b", "/a/b"), Some(String::new()));
        if !cfg!(windows) {
            assert_eq!(normalize_path(Path::new("/a/./b/../c/")), "/a/c");
        }
    }
}
